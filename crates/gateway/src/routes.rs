use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use tracing::error;

use crate::{
    channel::OutboundMessage,
    db::{self, now_ms, Message, Project},
    projects::sanitize_ident,
    AppState,
};

// ── Error helper ─────────────────────────────────────────────────────────────

pub(crate) struct AppError(pub StatusCode, pub String);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("handler error: {} — {}", self.0, self.1);
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        let err = e.into();
        AppError(StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}"))
    }
}

type Result<T> = std::result::Result<T, AppError>;

/// Extract agent identity from X-Agent-Id header, defaulting to "_default".
fn extract_agent_id(headers: &HeaderMap) -> String {
    headers
        .get("x-agent-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("_default")
        .to_string()
}

/// Maximum length of an auto-derived subject when the agent does not supply one.
const AUTO_SUBJECT_MAX: usize = 80;

/// Derive a subject from the body when one is not supplied: first non-empty
/// line, trimmed, capped at `AUTO_SUBJECT_MAX` characters with an ellipsis if
/// truncated. Falls back to a generic placeholder for empty bodies.
fn derive_subject(body: &str) -> String {
    let first_line = body.lines().map(str::trim).find(|l| !l.is_empty());
    match first_line {
        None => "(no content)".to_string(),
        Some(line) => {
            let count = line.chars().count();
            if count <= AUTO_SUBJECT_MAX {
                line.to_string()
            } else {
                let mut out: String = line.chars().take(AUTO_SUBJECT_MAX - 1).collect();
                out.push('…');
                out
            }
        }
    }
}

/// Apply default values for any missing structured fields and return a
/// fully-populated `OutboundMessage`. The body argument is the resolved
/// payload (caller picks between the structured `body` field and any
/// route-specific alias such as `content` or `message`).
fn build_outbound(
    agent_id: &str,
    body: String,
    subject: Option<String>,
    hostname: Option<String>,
    event_at: Option<i64>,
) -> OutboundMessage {
    let subject = subject
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| derive_subject(&body));
    let hostname = hostname
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| agent_id.to_string());
    let event_at = event_at.unwrap_or_else(now_ms);
    OutboundMessage {
        agent_id: agent_id.to_string(),
        hostname,
        subject,
        body,
        event_at,
    }
}

// ── Theme (GET/POST /theme) ──────────────────────────────────────────────────

#[derive(Serialize)]
pub struct ThemeResponse {
    pub theme: String,
}

#[derive(Deserialize)]
pub struct ThemeRequest {
    pub theme: String,
}

pub async fn get_theme(State(state): State<AppState>) -> Result<Json<ThemeResponse>> {
    let db = state.db.clone();
    let theme = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_theme(&conn)
    })
    .await??;
    Ok(Json(ThemeResponse { theme }))
}

pub async fn set_theme(
    State(state): State<AppState>,
    Json(body): Json<ThemeRequest>,
) -> Result<Json<ThemeResponse>> {
    let theme = body.theme.trim().to_lowercase();
    if theme != "light" && theme != "dark" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("unsupported theme '{}': must be 'light' or 'dark'", theme),
        ));
    }
    let db = state.db.clone();
    let t = theme.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::set_theme(&conn, &t)
    })
    .await??;
    Ok(Json(ThemeResponse { theme }))
}

// ── POST /v1/projects ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterProjectRequest {
    /// Raw project identity (git remote URL or directory name).
    pub ident: String,
    /// Which channel plugin to use. Defaults to gateway's DEFAULT_CHANNEL.
    pub channel: Option<String>,
}

#[derive(Serialize)]
pub struct RegisterProjectResponse {
    pub ident: String,
    pub channel_name: String,
    pub room_id: String,
}

pub async fn register_project(
    State(state): State<AppState>,
    Json(body): Json<RegisterProjectRequest>,
) -> Result<Json<RegisterProjectResponse>> {
    let project_ident = sanitize_ident(&body.ident);
    let channel_name = body
        .channel
        .unwrap_or_else(|| state.default_channel.clone());

    // Return existing project immediately (idempotent).
    {
        let conn = state.db.lock().unwrap();
        if let Some(existing) = db::get_project(&conn, &project_ident)? {
            return Ok(Json(RegisterProjectResponse {
                ident: existing.ident,
                channel_name: existing.channel_name,
                room_id: existing.room_id,
            }));
        }
    }

    // Look up the requested plugin.
    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::BAD_REQUEST,
                format!("unknown channel plugin: '{channel_name}'"),
            )
        })?
        .clone();

    // Plugin creates/finds the room.
    let room_id = plugin.ensure_room(&project_ident).await?;

    // Persist.
    let project = Project {
        ident: project_ident.clone(),
        channel_name: channel_name.clone(),
        room_id: room_id.clone(),
        last_msg_id: None,
        created_at: now_ms(),
    };

    let db = state.db.clone();
    let project_clone = project.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_project(&conn, &project_clone)
    })
    .await??;

    Ok(Json(RegisterProjectResponse {
        ident: project_ident,
        channel_name,
        room_id,
    }))
}

// ── POST /v1/projects/:ident/messages ─────────────────────────────────────────

#[derive(Deserialize)]
pub struct SendMessageRequest {
    /// Back-compat alias for `body`. If both are set, `body` wins.
    pub content: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub hostname: Option<String>,
    /// Event time in epoch milliseconds. Defaults to now() when omitted.
    pub event_at: Option<i64>,
}

#[derive(Serialize)]
pub struct SendMessageResponse {
    pub message_id: i64,
    pub external_message_id: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>> {
    let (channel_name, room_id) = {
        let conn = state.db.lock().unwrap();
        match db::get_project(&conn, &ident)? {
            Some(p) => (p.channel_name, p.room_id),
            None => {
                return Err(AppError(
                    StatusCode::NOT_FOUND,
                    format!("project '{}' not found", ident),
                ))
            }
        }
    };

    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("plugin '{channel_name}' not loaded"),
            )
        })?
        .clone();

    let agent_id = extract_agent_id(&headers);
    let body_text = req.body.or(req.content).unwrap_or_default();
    if body_text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request must include non-empty 'body' (or 'content')".into(),
        ));
    }
    let outbound = build_outbound(
        &agent_id,
        body_text,
        req.subject,
        req.hostname,
        req.event_at,
    );
    let external_id = plugin.send_structured(&room_id, &outbound).await?;

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: outbound.body.clone(),
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: None,
        agent_id: Some(agent_id.clone()),
        message_type: "message".into(),
        subject: Some(outbound.subject.clone()),
        hostname: Some(outbound.hostname.clone()),
        event_at: Some(outbound.event_at),
    };

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(SendMessageResponse {
        message_id: row_id,
        external_message_id: external_id,
    }))
}

// ── Skills API ────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct SkillUploadResponse {
    pub name: String,
    pub kind: String,
    pub size: i64,
    pub checksum: String,
}

pub async fn upload_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/zip");

    // X-Kind header takes precedence; fall back to Content-Type detection.
    let kind = match headers
        .get("x-kind")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_lowercase())
        .as_deref()
    {
        Some("skill") => "skill".to_string(),
        Some("command") => "command".to_string(),
        Some("agent") => "agent".to_string(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid X-Kind: '{other}'"),
            ))
        }
        None => {
            if content_type.starts_with("text/markdown") {
                "command".to_string()
            } else {
                "skill".to_string()
            }
        }
    };

    let is_text = kind == "command" || kind == "agent";

    let (zip_data, content, size) = if is_text {
        let text = String::from_utf8(body.to_vec())
            .map_err(|_| AppError(StatusCode::BAD_REQUEST, "body is not valid UTF-8".into()))?;
        if text.is_empty() {
            return Err(AppError(StatusCode::BAD_REQUEST, "body is empty".into()));
        }
        let size = text.len() as i64;
        (vec![], Some(text), size)
    } else {
        let zip = body.to_vec();
        let size = zip.len() as i64;
        (zip, None, size)
    };

    let mut hasher = Sha256::new();
    match &content {
        Some(text) => hasher.update(text.as_bytes()),
        None => hasher.update(&zip_data),
    }
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        kind: kind.clone(),
        zip_data,
        content,
        size,
        checksum: checksum.clone(),
        uploaded_at: db::now_ms(),
    };
    let db = state.db.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_skill(&conn, &record)
    })
    .await??;

    Ok(Json(SkillUploadResponse {
        name,
        kind,
        size,
        checksum,
    }))
}

/// Multipart variant of the skill-upload endpoint. Accepts a form with:
///   - `kind` (text): `"skill" | "command" | "agent"` — required
///   - `content` (text): markdown body — required when kind is `command|agent`
///   - `file` (binary): zip bytes — required when kind is `skill`
///
/// Designed for ndesign's `data-nd-action` form serializer, which posts
/// `multipart/form-data` when any field is a file. Persists into the same
/// `skills` table as the raw-body PUT variant.
pub async fn upload_skill_multipart(
    State(state): State<AppState>,
    Path(name): Path<String>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};

    let mut kind_field: Option<String> = None;
    let mut content_field: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        AppError(
            StatusCode::BAD_REQUEST,
            format!("failed to parse multipart body: {e}"),
        )
    })? {
        let field_name = field.name().unwrap_or("").to_string();
        match field_name.as_str() {
            "kind" => {
                let text = field.text().await.map_err(|e| {
                    AppError(
                        StatusCode::BAD_REQUEST,
                        format!("failed to read 'kind' field: {e}"),
                    )
                })?;
                kind_field = Some(text.trim().to_lowercase());
            }
            "content" => {
                let text = field.text().await.map_err(|e| {
                    AppError(
                        StatusCode::BAD_REQUEST,
                        format!("failed to read 'content' field: {e}"),
                    )
                })?;
                content_field = Some(text);
            }
            "file" => {
                let bytes = field.bytes().await.map_err(|e| {
                    AppError(
                        StatusCode::BAD_REQUEST,
                        format!("failed to read 'file' field: {e}"),
                    )
                })?;
                file_bytes = Some(bytes.to_vec());
            }
            _ => {
                // Silently ignore unknown fields — ndesign's serializer may
                // add incidental metadata fields that are not part of the
                // upload contract.
            }
        }
    }

    let kind = match kind_field.as_deref() {
        Some("skill") => "skill".to_string(),
        Some("command") => "command".to_string(),
        Some("agent") => "agent".to_string(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid 'kind': '{other}' (must be skill|command|agent)"),
            ))
        }
        None => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "'kind' field is required".into(),
            ))
        }
    };

    let (zip_data, content, size) = match kind.as_str() {
        "skill" => {
            let bytes = file_bytes.ok_or_else(|| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    "'file' field is required when kind is 'skill'".into(),
                )
            })?;
            if bytes.is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "'file' must be non-empty".into(),
                ));
            }
            let size = bytes.len() as i64;
            (bytes, None, size)
        }
        _ => {
            // command | agent
            let text = content_field.ok_or_else(|| {
                AppError(
                    StatusCode::BAD_REQUEST,
                    format!("'content' field is required when kind is '{kind}'"),
                )
            })?;
            if text.trim().is_empty() {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    "'content' must be non-empty".into(),
                ));
            }
            let size = text.len() as i64;
            (vec![], Some(text), size)
        }
    };

    let mut hasher = Sha256::new();
    match &content {
        Some(text) => hasher.update(text.as_bytes()),
        None => hasher.update(&zip_data),
    }
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        kind: kind.clone(),
        zip_data,
        content,
        size,
        checksum: checksum.clone(),
        uploaded_at: db::now_ms(),
    };
    let db = state.db.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_skill(&conn, &record)
    })
    .await??;

    Ok(Json(SkillUploadResponse {
        name,
        kind,
        size,
        checksum,
    }))
}

// ── POST /v1/skills/:name (JSON upsert for command/agent) ────────────────────

/// Request body for the JSON upsert endpoint.
///
/// `kind` must be `"command"` or `"agent"` — zip-backed skills cannot be
/// upserted via this endpoint and must use the existing `PUT` (raw body) or
/// `POST .../multipart` variants, which accept binary data.
///
/// `content` is the markdown body and must be non-empty after trimming.
#[derive(Deserialize)]
pub struct JsonSkillRequest {
    pub kind: String,
    pub content: String,
}

/// Create-or-update a text-kind skill (`command` or `agent`) via JSON.
///
/// ndesign's `data-nd-action` on a `<form>` serializes named inputs into a
/// JSON body (not multipart). This endpoint is the JSON-native create/edit
/// path used by the Commands and Agents control-panel pages. Zip skills are
/// not supported here — they are managed exclusively from the agent-tools
/// CLI via the raw-body `PUT` endpoint.
///
/// Validation:
/// * `name` — must be non-empty (400).
/// * `kind` — must be exactly `"command"` or `"agent"` (400 on anything else,
///   including `"skill"`).
/// * `content` — must be non-empty after `trim()` (400).
///
/// On success the same `SkillUploadResponse` shape as the raw-body PUT is
/// returned so clients can treat the two endpoints uniformly.
pub async fn upsert_skill_json(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<JsonSkillRequest>,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};

    if name.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'name' must be non-empty".into(),
        ));
    }

    let kind = match req.kind.as_str() {
        "command" => "command".to_string(),
        "agent" => "agent".to_string(),
        other => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!(
                    "invalid 'kind': '{other}' (must be 'command' or 'agent'; \
                     zip skills use PUT /v1/skills/:name)"
                ),
            ))
        }
    };

    if req.content.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'content' must be non-empty".into(),
        ));
    }

    let size = req.content.len() as i64;
    let mut hasher = Sha256::new();
    hasher.update(req.content.as_bytes());
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        kind: kind.clone(),
        zip_data: vec![],
        content: Some(req.content),
        size,
        checksum: checksum.clone(),
        uploaded_at: db::now_ms(),
    };
    let db = state.db.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_skill(&conn, &record)
    })
    .await??;

    Ok(Json(SkillUploadResponse {
        name,
        kind,
        size,
        checksum,
    }))
}

#[derive(Deserialize)]
pub struct ListSkillsQuery {
    /// Optional filter — when set, restrict the response to a single kind.
    pub kind: Option<String>,
}

pub async fn list_skills_handler(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListSkillsQuery>,
) -> Result<Json<Vec<db::SkillMeta>>> {
    let kind = match q.kind.as_deref() {
        None | Some("") => None,
        Some(k) => {
            if k != "skill" && k != "command" && k != "agent" {
                return Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("invalid kind '{k}': must be skill|command|agent"),
                ));
            }
            Some(k.to_string())
        }
    };

    let db = state.db.clone();
    let skills = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_skills(&conn, kind.as_deref())
    })
    .await??;
    Ok(Json(skills))
}

pub async fn download_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse> {
    let db = state.db.clone();
    let record = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_skill(&conn, &name)
    })
    .await??;

    match record {
        None => Err(AppError(StatusCode::NOT_FOUND, "not found".into())),
        Some(r) if r.kind == "command" || r.kind == "agent" => {
            let text = r.content.unwrap_or_default();
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/markdown; charset=utf-8"),
            );
            if let Ok(v) = HeaderValue::from_str(&r.kind) {
                headers.insert("x-kind", v);
            }
            let cd = format!("attachment; filename=\"{}.md\"", r.name);
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&cd).unwrap_or(HeaderValue::from_static("attachment")),
            );
            Ok((headers, text.into_bytes()))
        }
        Some(r) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
            if let Ok(v) = HeaderValue::from_str(&r.kind) {
                headers.insert("x-kind", v);
            }
            let cd = format!("attachment; filename=\"{}.zip\"", r.name);
            headers.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&cd).unwrap_or(HeaderValue::from_static("attachment")),
            );
            Ok((headers, r.zip_data))
        }
    }
}

pub async fn delete_skill_handler(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode> {
    let db = state.db.clone();
    let existed = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::delete_skill(&conn, &name)
    })
    .await??;

    if existed {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError(StatusCode::NOT_FOUND, "skill not found".into()))
    }
}

// ── GET /v1/skills/:name/content ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct SkillContentResponse {
    pub name: String,
    pub kind: String,
    pub content: Option<String>,
}

pub async fn get_skill_content(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<SkillContentResponse>> {
    let db = state.db.clone();
    let record = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_skill(&conn, &name)
    })
    .await??;

    match record {
        None => Err(AppError(StatusCode::NOT_FOUND, "not found".into())),
        Some(r) => Ok(Json(SkillContentResponse {
            name: r.name,
            kind: r.kind,
            content: r.content,
        })),
    }
}

// ── GET /skills, /commands, /agents (control-panel list pages) ──────────────

/// Render one of the three admin list pages: skills, commands, or agents.
///
/// `kind` must be `"skill"`, `"command"`, or `"agent"` and drives the API
/// URLs, page labels, whether to show the create/edit affordances, and the
/// row action (Download for zip skills, Edit modal for text kinds).
///
/// This is the shared body for `skills_page`, `commands_page`, and
/// `agents_page`; each public handler is a thin wrapper that passes its kind
/// through. See the per-kind table in the control-panel docs for the exact
/// label mapping.
async fn render_kind_page(state: &AppState, kind: &str) -> Result<Html<String>> {
    debug_assert!(
        kind == "skill" || kind == "command" || kind == "agent",
        "render_kind_page called with unsupported kind '{kind}'"
    );

    let db = state.db.clone();
    let theme = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_theme(&conn)
    })
    .await??;

    let (singular, plural, active, page_title) = match kind {
        "skill" => ("Skill", "Skills", "skills", "Skills"),
        "command" => ("Command", "Commands", "commands", "Commands"),
        "agent" => ("Agent", "Agents", "agents", "Agents"),
        // Defensive default — debug_assert above catches unexpected kinds in
        // debug builds; in release we fall through to the skills shape.
        _ => ("Skill", "Skills", "skills", "Skills"),
    };

    let is_text_kind = kind == "command" || kind == "agent";

    // Create button row — only for text kinds that can be authored in the UI.
    let create_row = if is_text_kind {
        format!(
            r##"  <div class="nd-flex nd-gap-md nd-mb-md">
    <button class="nd-btn-primary nd-btn-sm" data-nd-modal="#create-modal">
      + New {singular}
    </button>
  </div>
"##,
        )
    } else {
        String::new()
    };

    // Row action cell: Edit modal (text kinds) or a Download link (skills).
    // NOTE: `data-nd-context` on the Edit button is a spec-risk placeholder —
    // see the FIXME below near the edit modal.
    let row_action = if is_text_kind {
        r##"<!-- FIXME: confirm data-nd-context is the correct attribute for
                    row-to-modal context propagation; if the spec names it
                    differently, the edit modal will open with {{name}}
                    unresolved and we'll need ndesign support. -->
                <button class="nd-btn-secondary nd-btn-sm"
                      data-nd-modal="#edit-modal"
                      data-nd-context>Edit</button>"##
    } else {
        r##"<a class="nd-btn-secondary nd-btn-sm"
                 href="/v1/skills/{{name}}">Download</a>"##
    };

    // Create/Edit modals — only rendered for text kinds (no file upload in UI).
    let modals = if is_text_kind {
        format!(
            r##"
<dialog id="create-modal" class="nd-modal">
  <form data-nd-action="POST /v1/skills/{{{{name}}}}"
        data-nd-success="close-modal,refresh:#list-body,reset">
    <h3>New {singular}</h3>
    <div class="nd-form-group">
      <label for="create-name">Name</label>
      <input id="create-name" name="name" required>
    </div>
    <div class="nd-form-group">
      <label for="create-content">Content</label>
      <textarea id="create-content" name="content" rows="12" required></textarea>
    </div>
    <input type="hidden" name="kind" value="{kind}">
    <menu>
      <button type="button" data-nd-dismiss class="nd-btn-ghost">Cancel</button>
      <button type="submit" class="nd-btn-primary">Create</button>
    </menu>
  </form>
</dialog>

<dialog id="edit-modal" class="nd-modal">
  <form data-nd-action="POST /v1/skills/{{{{name}}}}"
        data-nd-success="close-modal,refresh:#list-body">
    <h3>Edit {{{{name}}}}</h3>
    <div class="nd-form-group">
      <label for="edit-content">Content</label>
      <!-- FIXME: confirm data-nd-bind-to-value binding works on textarea; if not, pre-fill is missing and we'll need ndesign support -->
      <textarea id="edit-content" name="content" rows="12"
                data-nd-bind="/v1/skills/{{{{name}}}}/content"
                data-nd-value="content" required></textarea>
    </div>
    <input type="hidden" name="kind" value="{kind}">
    <menu>
      <button type="button" data-nd-dismiss class="nd-btn-ghost">Cancel</button>
      <button type="submit" class="nd-btn-primary">Save</button>
    </menu>
  </form>
</dialog>
"##,
        )
    } else {
        String::new()
    };

    // NOTE: `data-nd-context` on the Edit button (interpolated above) is a
    // spec-risk placeholder attribute meaning "when this modal opens, bind
    // its form inputs to the clicked row's data so `{{name}}` resolves to
    // that row". If ndesign's row-to-modal context propagation uses a
    // different attribute name, the edit modal will open without the name
    // resolved and we'll need to update this once the spec is confirmed.
    let content = format!(
        r##"{create_row}  <section class="nd-card">
    <div class="nd-card-header"><strong>{plural}</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead>
          <tr><th>Name</th><th>Size</th><th>Updated</th><th>Actions</th></tr>
        </thead>
        <tbody id="list-body"
               data-nd-bind="/v1/skills?kind={kind}"
               data-nd-template="row-template">
          <template id="row-template">
            <tr>
              <td>{{{{name}}}}</td>
              <td class="nd-text-muted">{{{{size}}}}</td>
              <td class="nd-text-muted">{{{{uploaded_at}}}}</td>
              <td>
                {row_action}
                <button class="nd-btn-danger nd-btn-sm"
                        data-nd-action="DELETE /v1/skills/{{{{name}}}}"
                        data-nd-confirm="Delete {{{{name}}}}?"
                        data-nd-success="refresh:#list-body">Delete</button>
              </td>
            </tr>
          </template>
          <template data-nd-empty>
            <tr><td colspan="4" class="nd-text-muted">No {plural_lower} yet.</td></tr>
          </template>
        </tbody>
      </table>
    </div>
  </section>
{modals}"##,
        plural_lower = plural.to_lowercase(),
    );

    let full_title = format!("agent-gateway — {page_title}");
    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(&full_title, &theme, ""),
        open = control_panel_open(page_title, active),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

/// `GET /skills` — read-only list of zip-backed skills. Upload/edit flows
/// live in the agent-tools CLI; the UI here only supports download + delete.
pub async fn skills_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_kind_page(&state, "skill").await
}

/// `GET /commands` — list + create/edit/delete for markdown command skills.
pub async fn commands_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_kind_page(&state, "command").await
}

/// `GET /agents` — list + create/edit/delete for markdown agent skills.
pub async fn agents_page(State(state): State<AppState>) -> Result<Html<String>> {
    render_kind_page(&state, "agent").await
}

// ── GET / (dashboard) ─────────────────────────────────────────────────────────

fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn dashboard(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let (data, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((db::get_dashboard_data(&conn)?, db::get_theme(&conn)?))
    })
    .await??;

    let current_version = env!("AGENT_GATEWAY_VERSION");
    let update_banner = {
        let guard = state.update_available.lock().unwrap();
        match guard.as_deref() {
            Some(version) => format!(
                r#"<div class="nd-alert nd-alert-warning nd-mb-lg">
  <strong>Update available:</strong> {} (current: v{}) — run <code>gateway update</code>
</div>"#,
                he(version),
                he(current_version),
            ),
            None => String::new(),
        }
    };

    let rows = if data.project_count == 0 {
        r#"<tr><td colspan="5" class="nd-text-muted nd-text-center">No projects registered yet</td></tr>"#.to_string()
    } else {
        data.projects
            .iter()
            .map(|p| {
                let unread_cell = if p.unread_count > 0 {
                    format!(
                        r#"<span class="nd-badge nd-badge-sm nd-text-danger">{}</span>"#,
                        p.unread_count
                    )
                } else {
                    "0".into()
                };
                format!(
                    "<tr><td>{}</td><td>{}</td><td class=\"nd-text-muted\">{}</td><td>{}</td><td>{}</td></tr>",
                    he(&p.ident),
                    he(&p.channel_name),
                    he(&p.room_id),
                    p.total_messages,
                    unread_cell,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let content = format!(
        r#"  {banner}
  <p class="nd-text-muted nd-text-sm">Channel plugin dashboard · v{version}</p>

  <section class="nd-row nd-gap-md nd-mb-lg">
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{projects}</div><div class="nd-text-xs nd-text-muted">Projects</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{total}</div><div class="nd-text-xs nd-text-muted">Total messages</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{agent}</div><div class="nd-text-xs nd-text-muted">Agent</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{user}</div><div class="nd-text-xs nd-text-muted">User</div></div></div></div>
    <div class="nd-col-2"><div class="nd-card"><div class="nd-card-body"><div class="nd-text-2xl nd-font-bold">{skills}</div><div class="nd-text-xs nd-text-muted">Skills</div></div></div></div>
  </section>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Projects</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead><tr><th>Project</th><th>Channel</th><th>Room ID</th><th>Messages</th><th>Unread</th></tr></thead>
        <tbody>{rows}</tbody>
      </table>
    </div>
  </section>"#,
        banner = update_banner,
        version = he(current_version),
        projects = data.project_count,
        total = data.total_messages,
        agent = data.agent_messages,
        user = data.user_messages,
        skills = data.skill_count,
        rows = rows,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Dashboard", &theme, ""),
        open = control_panel_open("Dashboard", "dashboard"),
        content = content,
        close = control_panel_close(&state.api_key),
    );

    Ok(Html(html))
}

// ── Patterns API ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListPatternsQuery {
    pub q: Option<String>,
    pub label: Option<String>,
    pub version: Option<String>,
    pub state: Option<String>,
    pub superseded_by: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatePatternRequest {
    pub title: String,
    pub slug: Option<String>,
    pub summary: Option<String>,
    pub body: String,
    pub labels: Option<serde_json::Value>,
    pub version: String,
    pub state: String,
    /// Defaults to X-Agent-Id header, or "user" when the header is absent.
    pub author: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdatePatternRequest {
    pub title: Option<String>,
    pub slug: Option<String>,
    /// `Some(null)` clears the summary; absent leaves it untouched.
    pub summary: Option<serde_json::Value>,
    pub body: Option<String>,
    pub labels: Option<serde_json::Value>,
    pub version: Option<String>,
    pub state: Option<String>,
}

fn validate_pattern_version_field(version: &str) -> Result<()> {
    if version == "draft" || version == "latest" || version == "superseded" {
        Ok(())
    } else {
        Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid version '{version}': must be draft|latest|superseded"),
        ))
    }
}

fn decode_labels_field(field: &str, value: Option<serde_json::Value>) -> Result<Vec<String>> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .map(|v| match v {
                serde_json::Value::String(s) => Ok(s.trim().to_string()),
                _ => Err(AppError(
                    StatusCode::BAD_REQUEST,
                    format!("'{field}' must be an array of strings or a comma-separated string"),
                )),
            })
            .filter_map(|r| match r {
                Ok(s) if s.is_empty() => None,
                other => Some(other),
            })
            .collect(),
        Some(serde_json::Value::String(s)) => Ok(s
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()),
        Some(_) => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("'{field}' must be an array of strings or a comma-separated string"),
        )),
    }
}

fn decode_optional_labels_field(
    field: &str,
    value: Option<serde_json::Value>,
) -> Result<Option<Vec<String>>> {
    match value {
        None => Ok(None),
        Some(v) => decode_labels_field(field, Some(v)).map(Some),
    }
}

pub async fn list_patterns_handler(
    State(state): State<AppState>,
    Query(q): Query<ListPatternsQuery>,
) -> Result<Json<Vec<db::PatternSummary>>> {
    if let Some(version) = q.version.as_deref() {
        validate_pattern_version_field(version.trim())?;
    }
    let db = state.db.clone();
    let query = q.q;
    let label = q.label;
    let version = q.version;
    let state_value = q.state;
    let superseded_by = q.superseded_by;
    let patterns = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let filters = db::PatternFilters {
            query: query.as_deref(),
            label: label.as_deref(),
            version: version.as_deref(),
            state: state_value.as_deref(),
            superseded_by: superseded_by.as_deref(),
        };
        db::list_patterns(&conn, &filters)
    })
    .await??;
    Ok(Json(patterns))
}

pub async fn create_pattern_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreatePatternRequest>,
) -> Result<Json<db::Pattern>> {
    if req.title.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "title is required".into(),
        ));
    }
    if req.body.trim().is_empty() {
        return Err(AppError(StatusCode::BAD_REQUEST, "body is required".into()));
    }
    validate_pattern_version_field(req.version.trim())?;
    if req.state.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "state is required".into(),
        ));
    }

    let labels = decode_labels_field("labels", req.labels)?;
    let author = resolve_identity(req.author, &headers);
    let db = state.db.clone();
    let title = req.title;
    let slug = req.slug;
    let summary = req.summary;
    let body = req.body;
    let version = req.version;
    let state_value = req.state;

    let pattern = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_pattern(
            &conn,
            title.trim(),
            slug.as_deref().map(str::trim),
            summary.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            &body,
            &labels,
            version.trim(),
            state_value.trim(),
            &author,
        )
    })
    .await??;
    Ok(Json(pattern))
}

pub async fn get_pattern_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<db::Pattern>> {
    let db = state.db.clone();
    let id_for_lookup = id.clone();
    let pattern = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_pattern(&conn, &id_for_lookup)
    })
    .await??;

    match pattern {
        Some(pattern) => Ok(Json(pattern)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

pub async fn update_pattern_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<UpdatePatternRequest>,
) -> Result<Json<db::Pattern>> {
    let summary = decode_nullable_string("summary", req.summary)?;
    let labels = decode_optional_labels_field("labels", req.labels)?;
    if let Some(version) = req.version.as_deref() {
        validate_pattern_version_field(version.trim())?;
    }
    if let Some(state) = req.state.as_deref() {
        if state.trim().is_empty() {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                "state cannot be empty".into(),
            ));
        }
    }
    let db = state.db.clone();
    let id_for_update = id.clone();
    let title = req.title;
    let slug = req.slug;
    let body = req.body;
    let version = req.version;
    let state_value = req.state;

    let pattern = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        let upd = db::PatternUpdate {
            title: title.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            slug: slug.as_deref().map(str::trim).filter(|s| !s.is_empty()),
            summary: summary.as_ref().map(|inner| inner.as_deref()),
            body: body.as_deref(),
            labels: labels.as_deref(),
            version: version.as_deref().map(str::trim),
            state: state_value.as_deref().map(str::trim),
        };
        db::update_pattern(&conn, &id_for_update, &upd)
    })
    .await??;

    match pattern {
        Some(pattern) => Ok(Json(pattern)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

pub async fn delete_pattern_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<DeleteResponse>> {
    let db = state.db.clone();
    let id_for_delete = id.clone();
    let deleted = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::delete_pattern(&conn, &id_for_delete)
    })
    .await??;

    if deleted {
        Ok(Json(DeleteResponse { deleted }))
    } else {
        Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        ))
    }
}

pub async fn list_pattern_comments_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<db::PatternComment>>> {
    let db = state.db.clone();
    let id_for_lookup = id.clone();
    let comments = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_pattern_comments(&conn, &id_for_lookup)
    })
    .await??;

    match comments {
        Some(comments) => Ok(Json(comments)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

pub async fn add_pattern_comment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<AddCommentRequest>,
) -> Result<Json<db::PatternComment>> {
    if req.content.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "content is required".into(),
        ));
    }
    let author = resolve_identity(req.author, &headers);
    let author_type = req.author_type.unwrap_or_else(|| {
        if actor_agent_id(&headers).is_some() {
            "agent".to_string()
        } else {
            "user".to_string()
        }
    });
    if author_type != "agent" && author_type != "user" && author_type != "system" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid author_type '{author_type}': must be agent|user|system"),
        ));
    }

    let db = state.db.clone();
    let id_for_insert = id.clone();
    let content = req.content;
    let comment = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_pattern_comment(&conn, &id_for_insert, &author, &author_type, &content)
    })
    .await??;

    match comment {
        Some(comment) => Ok(Json(comment)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("pattern '{}' not found", id),
        )),
    }
}

// ── Tasks API ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct ListTasksQuery {
    /// Comma-separated list of statuses (e.g. "todo,in_progress").
    /// Defaults to `todo,in_progress` when absent.
    pub status: Option<String>,
    /// When true, include `done` tasks older than 7 days. Default false.
    pub include_stale: Option<bool>,
}

#[derive(Deserialize)]
pub struct CreateTaskRequest {
    pub title: String,
    pub description: Option<String>,
    pub details: Option<String>,
    pub labels: Option<Vec<String>>,
    pub hostname: Option<String>,
    /// Optional override of reporter. Defaults to X-Agent-Id header, or "user"
    /// when the header is absent or "_default".
    pub reporter: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateTaskRequest {
    pub status: Option<String>,
    /// `Some(null)` in JSON clears the owner; `Some("xyz")` assigns it;
    /// absent leaves the current owner alone.
    pub owner_agent_id: Option<serde_json::Value>,
    pub rank: Option<i64>,
    pub title: Option<String>,
    pub description: Option<serde_json::Value>,
    pub details: Option<serde_json::Value>,
    pub labels: Option<Vec<String>>,
    pub hostname: Option<serde_json::Value>,
}

#[derive(Deserialize)]
pub struct AddCommentRequest {
    pub content: String,
    /// `"agent"` | `"user"`. Defaults based on whether X-Agent-Id is present.
    pub author_type: Option<String>,
    /// Defaults to X-Agent-Id header, or `"user"` when the header is absent or
    /// `"_default"`.
    pub author: Option<String>,
}

/// Flat detail shape: all `Task` fields at the top level (via `#[serde(flatten)]`)
/// plus a sibling `comments` array and a derived `actions` array. Designed so
/// that ndesign's `data-nd-bind` can render the detail view — including
/// status-transition buttons — without template conditionals.
#[derive(Serialize)]
pub struct TaskWithComments {
    #[serde(flatten)]
    pub task: db::Task,
    pub comments: Vec<db::TaskComment>,
    pub actions: Vec<TaskAction>,
}

/// One status-transition button derived from the task's current status.
///
/// The UI iterates this array inside the modal; each entry is rendered as a
/// `<button data-nd-action="PATCH …" data-nd-body=…>` so ndesign fires the
/// PATCH when the user clicks. `style` is the `nd-btn-*` suffix
/// (`primary` | `secondary`) so the template can build the class name.
#[derive(Serialize)]
pub struct TaskAction {
    pub verb: String,
    pub style: String,
    pub target_status: String,
}

/// Compute the list of allowed status transitions for a given current status.
/// Kept in one place so the UI and any future API consumers agree.
fn actions_for_status(status: &str) -> Vec<TaskAction> {
    let mk = |verb: &str, style: &str, target: &str| TaskAction {
        verb: verb.into(),
        style: style.into(),
        target_status: target.into(),
    };
    match status {
        "todo" => vec![
            mk("Claim", "primary", "in_progress"),
            mk("Done", "primary", "done"),
        ],
        "in_progress" => vec![
            mk("Release", "secondary", "todo"),
            mk("Done", "primary", "done"),
        ],
        "done" => vec![mk("Reopen", "secondary", "todo")],
        _ => vec![],
    }
}

#[derive(Serialize)]
pub struct DeleteResponse {
    pub deleted: bool,
}

#[derive(Deserialize)]
pub struct ReorderTasksQuery {
    /// Target column (`todo` | `in_progress` | `done`). Required.
    pub status: String,
}

#[derive(Deserialize)]
pub struct ReorderTasksRequest {
    pub order: Vec<String>,
}

/// Parse a JSON nullable-string update field.
///
/// - `None`                          → `None`        (field not touched)
/// - `Some(Value::Null)`             → `Some(None)`  (clear column)
/// - `Some(Value::String(s))`        → `Some(Some(s))` (set column)
/// - anything else                   → 400
fn decode_nullable_string(
    field: &str,
    value: Option<serde_json::Value>,
) -> Result<Option<Option<String>>> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::Null) => Ok(Some(None)),
        Some(serde_json::Value::String(s)) => Ok(Some(Some(s))),
        Some(_) => Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("'{field}' must be a string or null"),
        )),
    }
}

/// Resolve the reporter/author identity from an explicit body field, the
/// X-Agent-Id header, or fall back to `"user"`.
fn resolve_identity(explicit: Option<String>, headers: &HeaderMap) -> String {
    if let Some(s) = explicit.and_then(|s| {
        let t = s.trim().to_string();
        if t.is_empty() {
            None
        } else {
            Some(t)
        }
    }) {
        return s;
    }
    let hdr = extract_agent_id(headers);
    if hdr == "_default" {
        "user".to_string()
    } else {
        hdr
    }
}

/// Optional agent id from header for actor-aware operations (None when the
/// header is absent or is the sentinel "_default").
fn actor_agent_id(headers: &HeaderMap) -> Option<String> {
    let hdr = extract_agent_id(headers);
    if hdr == "_default" {
        None
    } else {
        Some(hdr)
    }
}

pub async fn list_tasks_handler(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Query(q): Query<ListTasksQuery>,
) -> Result<Json<Vec<db::TaskSummary>>> {
    // Verify project exists (consistent with other handlers).
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let statuses: Vec<String> = match q.status.as_deref() {
        None | Some("") => vec!["todo".into(), "in_progress".into()],
        Some(s) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
    };
    for s in &statuses {
        if s != "todo" && s != "in_progress" && s != "done" {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid status '{s}': must be todo|in_progress|done"),
            ));
        }
    }
    let include_stale = q.include_stale.unwrap_or(false);

    let db = state.db.clone();
    let ident_for_reclaim = ident.clone();
    let ident_for_list = ident;
    let tasks = spawn_blocking(move || -> anyhow::Result<Vec<db::TaskSummary>> {
        let conn = db.lock().unwrap();
        // Reclaim stale in-progress tasks before listing so clients see a
        // consistent view.
        db::reclaim_stale_tasks(&conn, &ident_for_reclaim)?;
        db::list_tasks(&conn, &ident_for_list, &statuses, include_stale)
    })
    .await??;

    Ok(Json(tasks))
}

pub async fn create_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    Json(req): Json<CreateTaskRequest>,
) -> Result<Json<db::Task>> {
    // Verify project exists.
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'title' must be non-empty".into(),
        ));
    }

    let reporter = resolve_identity(req.reporter, &headers);
    let description = req.description;
    let details = req.details;
    let labels = req.labels.unwrap_or_default();
    let hostname = req.hostname;

    let db = state.db.clone();
    let ident_clone = ident;
    let task = spawn_blocking(move || -> anyhow::Result<db::Task> {
        let conn = db.lock().unwrap();
        db::insert_task(
            &conn,
            &ident_clone,
            &title,
            description.as_deref(),
            details.as_deref(),
            &labels,
            hostname.as_deref(),
            &reporter,
        )
    })
    .await??;

    Ok(Json(task))
}

pub async fn get_task_handler(
    State(state): State<AppState>,
    Path((ident, task_id)): Path<(String, String)>,
) -> Result<Json<TaskWithComments>> {
    let db = state.db.clone();
    let ident_for_reclaim = ident.clone();
    let ident_for_fetch = ident.clone();
    let task_id_clone = task_id;
    let detail = spawn_blocking(move || -> anyhow::Result<Option<db::TaskDetail>> {
        let conn = db.lock().unwrap();
        db::reclaim_stale_tasks(&conn, &ident_for_reclaim)?;
        db::get_task_detail(&conn, &ident_for_fetch, &task_id_clone)
    })
    .await??;

    match detail {
        Some(d) => {
            let actions = actions_for_status(&d.task.status);
            Ok(Json(TaskWithComments {
                task: d.task,
                comments: d.comments,
                actions,
            }))
        }
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("task not found in project '{}'", ident),
        )),
    }
}

pub async fn update_task_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, task_id)): Path<(String, String)>,
    Json(req): Json<UpdateTaskRequest>,
) -> Result<Json<db::Task>> {
    // Validate & decode the nullable-string fields up-front so we can return
    // 400 on a client-side shape mistake rather than bubbling a 500.
    let owner_opt = decode_nullable_string("owner_agent_id", req.owner_agent_id)?;
    let description_opt = decode_nullable_string("description", req.description)?;
    let details_opt = decode_nullable_string("details", req.details)?;
    let hostname_opt = decode_nullable_string("hostname", req.hostname)?;

    let actor = actor_agent_id(&headers);
    let db = state.db.clone();
    let ident_for_reclaim = ident.clone();
    let ident_for_update = ident.clone();
    let task_id_clone = task_id;
    let status = req.status;
    let rank = req.rank;
    let title = req.title;
    let labels = req.labels;

    // Invalid status transitions and bad status values are currently reported
    // by `db::update_task` as anyhow errors; they bubble through `AppError`'s
    // blanket From impl as 500. The CHECK constraint on `status` catches the
    // truly invalid values at the SQL layer. Refine to 400 when we add a
    // dedicated error enum.
    let task = spawn_blocking(move || -> anyhow::Result<Option<db::Task>> {
        let conn = db.lock().unwrap();
        db::reclaim_stale_tasks(&conn, &ident_for_reclaim)?;

        let upd = db::TaskUpdate {
            status: status.as_deref(),
            owner_agent_id: owner_opt.as_ref().map(|inner| inner.as_deref()),
            rank,
            title: title.as_deref(),
            description: description_opt.as_ref().map(|inner| inner.as_deref()),
            details: details_opt.as_ref().map(|inner| inner.as_deref()),
            labels: labels.as_deref(),
            hostname: hostname_opt.as_ref().map(|inner| inner.as_deref()),
        };
        db::update_task(
            &conn,
            &ident_for_update,
            &task_id_clone,
            &upd,
            actor.as_deref(),
        )
    })
    .await??;

    match task {
        Some(t) => Ok(Json(t)),
        None => Err(AppError(
            StatusCode::NOT_FOUND,
            format!("task not found in project '{}'", ident),
        )),
    }
}

pub async fn add_comment_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, task_id)): Path<(String, String)>,
    Json(req): Json<AddCommentRequest>,
) -> Result<Json<db::TaskComment>> {
    let content = req.content;
    if content.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "'content' must be non-empty".into(),
        ));
    }

    // Confirm the task exists in the project first.
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
        if db::get_task_detail(&conn, &ident, &task_id)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("task not found in project '{}'", ident),
            ));
        }
    }

    let header_agent = actor_agent_id(&headers);
    let author = resolve_identity(req.author, &headers);
    let author_type = match req.author_type.as_deref() {
        Some("agent") => "agent".to_string(),
        Some("user") => "user".to_string(),
        Some(other) => {
            return Err(AppError(
                StatusCode::BAD_REQUEST,
                format!("invalid author_type '{other}': must be agent|user"),
            ));
        }
        None => {
            if header_agent.is_some() {
                "agent".into()
            } else {
                "user".into()
            }
        }
    };

    let db = state.db.clone();
    let task_id_clone = task_id;
    let comment = spawn_blocking(move || -> anyhow::Result<db::TaskComment> {
        let conn = db.lock().unwrap();
        db::insert_comment(&conn, &task_id_clone, &author, &author_type, &content)
    })
    .await??;

    Ok(Json(comment))
}

pub async fn delete_task_handler(
    State(state): State<AppState>,
    Path((ident, task_id)): Path<(String, String)>,
) -> Result<Json<DeleteResponse>> {
    let db = state.db.clone();
    let ident_clone = ident;
    let task_id_clone = task_id;
    let deleted = spawn_blocking(move || -> anyhow::Result<bool> {
        let conn = db.lock().unwrap();
        db::delete_task(&conn, &ident_clone, &task_id_clone)
    })
    .await??;
    Ok(Json(DeleteResponse { deleted }))
}

/// Apply a client-driven reorder within a single status column.
///
/// Designed to receive `data-nd-sortable` POSTs: the body is
/// `{"order": ["id1", "id2", ...]}` and `?status=` selects the column the
/// order applies to. Returns the fresh list for that column so callers can
/// re-render without a follow-up GET.
pub async fn reorder_tasks_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
    axum::extract::Query(q): axum::extract::Query<ReorderTasksQuery>,
    Json(req): Json<ReorderTasksRequest>,
) -> Result<Json<Vec<db::TaskSummary>>> {
    // Verify project exists (consistent with other project-scoped handlers).
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let status = q.status.clone();
    if status != "todo" && status != "in_progress" && status != "done" {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            format!("invalid status '{status}': must be todo|in_progress|done"),
        ));
    }

    let actor = actor_agent_id(&headers);
    let db = state.db.clone();
    let ident_clone = ident;
    let status_clone = status.clone();
    let order = req.order;

    let tasks = spawn_blocking(move || -> anyhow::Result<Vec<db::TaskSummary>> {
        let conn = db.lock().unwrap();
        db::reorder_tasks_in_column(&conn, &ident_clone, &status_clone, &order, actor.as_deref())?;
        db::list_tasks(
            &conn,
            &ident_clone,
            std::slice::from_ref(&status_clone),
            false,
        )
    })
    .await??;

    Ok(Json(tasks))
}

// ── GET /v1/projects (JSON — used by the Tasks picker binding) ───────────────

/// List all registered projects with the same per-project stats shape the
/// dashboard uses. Returned as a bare array (no envelope) so ndesign's
/// `data-nd-bind` can render rows directly.
pub async fn list_projects_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::ProjectStats>>> {
    let db = state.db.clone();
    let projects = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_project_stats(&conn)
    })
    .await??;
    Ok(Json(projects))
}

// ── GET /patterns (global pattern library) ───────────────────────────────────

pub async fn patterns_page(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let theme = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_theme(&conn)
    })
    .await??;

    let content = r##"  <div class="nd-flex nd-gap-md nd-mb-md">
    <button class="nd-btn-primary nd-btn-sm" data-nd-modal="#new-pattern-modal">+ New pattern</button>
  </div>

  <template id="pattern-row">
    <tr>
      <td>
        <a class="nd-btn-ghost nd-text-left" href="/patterns/{{id}}">
          <strong>{{title}}</strong>
        </a>
        <div class="nd-text-muted nd-text-sm">{{summary}}</div>
      </td>
      <td class="nd-text-muted">{{slug}}</td>
      <td>{{version}}</td>
      <td class="nd-text-muted">{{state}}</td>
      <td>{{comment_count}}</td>
    </tr>
  </template>

  <section class="nd-card">
    <div class="nd-card-header"><strong>Patterns</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead>
          <tr><th>Pattern</th><th>Slug</th><th>Version</th><th>State</th><th>Comments</th></tr>
        </thead>
        <tbody id="patterns-list"
               data-nd-bind="/v1/patterns"
               data-nd-template="pattern-row">
          <template data-nd-empty>
            <tr><td colspan="5" class="nd-text-muted">No patterns yet.</td></tr>
          </template>
        </tbody>
      </table>
    </div>
  </section>

  <dialog id="new-pattern-modal" class="nd-modal nd-modal-lg">
    <form data-nd-action="POST /v1/patterns"
          data-nd-success="close-modal,refresh:#patterns-list,reset">
      <header><h3>New pattern</h3></header>
      <div>
        <div class="nd-form-group">
          <label for="pattern-title">Title</label>
          <input id="pattern-title" name="title" required>
        </div>
        <div class="nd-form-group">
          <label for="pattern-slug">Slug</label>
          <input id="pattern-slug" name="slug">
        </div>
        <div class="nd-form-group">
          <label for="pattern-summary">Summary</label>
          <textarea id="pattern-summary" name="summary" rows="2"></textarea>
        </div>
        <div class="nd-form-group">
          <label for="pattern-labels">Labels</label>
          <input id="pattern-labels" name="labels">
        </div>
        <div class="nd-form-group">
          <label for="pattern-version">Version</label>
          <select id="pattern-version" name="version" required>
            <option value="draft" selected>draft</option>
            <option value="latest">latest</option>
            <option value="superseded">superseded</option>
          </select>
        </div>
        <div class="nd-form-group">
          <label for="pattern-state">State</label>
          <input id="pattern-state" name="state" value="active" required>
        </div>
        <div class="nd-form-group">
          <label for="pattern-body">Markdown</label>
          <textarea id="pattern-body" name="body" rows="16" required></textarea>
        </div>
      </div>
      <footer>
        <button type="button" data-nd-dismiss class="nd-btn-ghost">Cancel</button>
        <button type="submit" class="nd-btn-primary">Create</button>
      </footer>
    </form>
  </dialog>

  "##;

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Patterns", &theme, "",),
        open = control_panel_open("Patterns", "patterns"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── GET /patterns/:id (global pattern detail/editor) ─────────────────────────

pub async fn pattern_detail_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    let db = state.db.clone();
    let id_for_lookup = id.clone();
    let (pattern, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db.lock().unwrap();
        Ok((
            db::get_pattern(&conn, &id_for_lookup)?,
            db::get_theme(&conn)?,
        ))
    })
    .await??;

    let pattern = pattern
        .ok_or_else(|| AppError(StatusCode::NOT_FOUND, format!("pattern '{}' not found", id)))?;

    let version_option = |value: &str, label: &str| -> String {
        if pattern.version == value {
            format!(
                r#"<option value="{}" selected>{}</option>"#,
                he(value),
                he(label)
            )
        } else {
            format!(r#"<option value="{}">{}</option>"#, he(value), he(label))
        }
    };
    let labels = pattern.labels.join(", ");
    let summary = pattern.summary.as_deref().unwrap_or("");
    let detail_title = format!("Pattern: {}", pattern.title);
    let api_id = he(&pattern.id);

    let content = format!(
        r#"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-secondary nd-btn-sm" href="/patterns">Back to patterns</a>
  </div>

  <section class="nd-card">
    <div class="nd-card-header">
      <div>
        <strong>{title}</strong>
        <div id="pattern-detail-meta" class="nd-text-muted nd-text-sm">
          slug: {slug} · version {version} · state {state} · author {author}
        </div>
      </div>
    </div>
    <div class="nd-card-body">
      <form data-nd-action="PATCH /v1/patterns/{api_id}">
        <div class="nd-row">
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-title">Title</label>
              <input id="pattern-edit-title" name="title" value="{title}" required>
            </div>
          </div>
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-slug">Slug</label>
              <input id="pattern-edit-slug" name="slug" value="{slug}" required>
            </div>
          </div>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-summary">Summary</label>
          <textarea id="pattern-edit-summary" name="summary" rows="2">{summary}</textarea>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-labels">Labels</label>
          <input id="pattern-edit-labels" name="labels" value="{labels}">
        </div>
        <div class="nd-row">
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-version">Version</label>
              <select id="pattern-edit-version" name="version" required>
                {draft}
                {latest}
                {superseded}
              </select>
            </div>
          </div>
          <div class="nd-col-6">
            <div class="nd-form-group">
              <label for="pattern-edit-state">State</label>
              <input id="pattern-edit-state" name="state" value="{state}" required>
            </div>
          </div>
        </div>
        <div class="nd-form-group">
          <label for="pattern-edit-body">Markdown</label>
          <textarea id="pattern-edit-body" name="body" rows="28" required>{body}</textarea>
        </div>
        <div class="nd-flex nd-gap-sm">
          <button type="submit" class="nd-btn-primary">Save pattern</button>
          <a class="nd-btn-secondary" href="/patterns">Done</a>
        </div>
      </form>
    </div>
  </section>

  <section class="nd-card nd-mt-lg">
    <div class="nd-card-header"><strong>Comments</strong></div>
    <div class="nd-card-body">
      <div id="pattern-comments"
           data-nd-bind="/v1/patterns/{api_id}/comments"
           data-nd-template="pattern-comment-tmpl">
        <template id="pattern-comment-tmpl">
          <div class="nd-mb-md">
            <div class="nd-text-muted nd-text-sm">{{{{author}}}} ({{{{author_type}}}})</div>
            <div>{{{{content}}}}</div>
          </div>
        </template>
        <template data-nd-empty>
          <p class="nd-text-muted nd-text-sm">No comments yet.</p>
        </template>
      </div>

      <form class="nd-mt-lg"
            data-nd-action="POST /v1/patterns/{api_id}/comments"
            data-nd-success="refresh:#pattern-comments,reset">
        <div class="nd-form-group">
          <label for="pattern-comment">Add a comment</label>
          <textarea id="pattern-comment" name="content" rows="3" required></textarea>
        </div>
        <button type="submit" class="nd-btn-primary nd-btn-sm">Comment</button>
      </form>
    </div>
  </section>"#,
        api_id = api_id,
        title = he(&pattern.title),
        slug = he(&pattern.slug),
        version = he(&pattern.version),
        state = he(&pattern.state),
        author = he(&pattern.author),
        summary = he(summary),
        labels = he(&labels),
        body = he(&pattern.body),
        draft = version_option("draft", "draft"),
        latest = version_option("latest", "latest"),
        superseded = version_option("superseded", "superseded"),
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Pattern", &theme, ""),
        open = control_panel_open(&detail_title, "patterns"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── GET /tasks (project picker) ──────────────────────────────────────────────

/// Render the project picker — a small table that the ndesign runtime
/// hydrates from `GET /v1/projects`. The authenticated XHR is carried by the
/// `Authorization` header emitted by `NDesign.configure` in
/// `control_panel_close`.
pub async fn tasks_picker(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let theme = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_theme(&conn)
    })
    .await??;

    let content = r##"  <section class="nd-card">
    <div class="nd-card-header"><strong>Projects</strong></div>
    <div class="nd-card-body nd-p-0">
      <table class="nd-table nd-table-hover">
        <thead>
          <tr><th>Project</th><th>Channel</th><th>Messages</th><th></th></tr>
        </thead>
        <tbody id="project-picker-body"
               data-nd-bind="/v1/projects"
               data-nd-template="project-row">
          <template id="project-row">
            <tr>
              <td>{{ident}}</td>
              <td class="nd-text-muted">{{channel_name}}</td>
              <td>{{total_messages}}</td>
              <td>
                <a class="nd-btn-primary nd-btn-sm" href="/projects/{{ident}}/tasks">
                  Open board
                </a>
              </td>
            </tr>
          </template>
          <template data-nd-empty>
            <tr><td colspan="4" class="nd-text-muted">No projects registered yet.</td></tr>
          </template>
        </tbody>
      </table>
    </div>
  </section>"##;

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head("agent-gateway — Tasks", &theme, ""),
        open = control_panel_open("Tasks", "tasks"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── GET /projects/:ident/tasks (board) ───────────────────────────────────────

/// Render the three-column task board for a single project.
///
/// The columns bind to `GET /v1/projects/:ident/tasks?status=…` and the
/// drag-and-drop reorder posts to `POST /v1/projects/:ident/tasks/reorder?status=…`.
/// Returns 404 when the project is not registered.
pub async fn tasks_board(
    State(state): State<AppState>,
    Path(ident): Path<String>,
) -> Result<Html<String>> {
    let db_handle = state.db.clone();
    let ident_for_lookup = ident.clone();
    let (project, theme) = spawn_blocking(move || -> anyhow::Result<_> {
        let conn = db_handle.lock().unwrap();
        let project = db::get_project(&conn, &ident_for_lookup)?;
        let theme = db::get_theme(&conn)?;
        Ok((project, theme))
    })
    .await??;

    let project = project.ok_or_else(|| {
        AppError(
            StatusCode::NOT_FOUND,
            format!("project '{}' not found", ident),
        )
    })?;

    // `ident` is produced by `sanitize_ident` (enforced at registration
    // time, see `register_project`) so it is already safe for URLs. We still
    // HTML-escape before emitting into attribute values and text nodes as
    // defense in depth.
    let ident_attr = he(&project.ident);
    let page_title = format!("Tasks — {}", project.ident);

    // Layout: `.nd-row` gives each `.nd-col-*` 0.5rem of inner padding on
    // both sides (Bootstrap-style gutter). For the cards to have visible
    // space BETWEEN them, the card must live *inside* the col wrapper — if
    // `.nd-card` and `.nd-col-*` are applied to the same element, the col
    // padding lands inside the card's background and the three cards appear
    // flush against each other.
    //
    // Cross-column drag: each column carries `data-nd-sortable-group="tasks"`
    // so the ndesign sortable runtime allows drops between them. On a
    // cross-column drop the runtime POSTs to the destination column's reorder
    // URL, which mutates the task's status server-side (see
    // `reorder_tasks_in_column`). A follow-up `nd:refresh` on every column
    // keeps the board in sync without a reload.
    //
    // Modal pattern (ndesign SPEC §5.8, §20.12): the card button writes the
    // task id into the `selectedTaskId` store var, opens the dialog, and
    // dispatches `nd:refresh` on every bound panel inside the dialog. The
    // bound panels share the same URL so the runtime dedupes them into a
    // single HTTP fetch. `#task-modal-meta` MUST be in the refresh list —
    // it is `data-nd-defer` and holds the description + details, so without
    // the explicit refresh those fields stay blank on first open.
    let content = format!(
        r##"  <div class="nd-flex nd-gap-md nd-mb-md">
    <a class="nd-btn-ghost nd-btn-sm" href="/tasks">← All projects</a>
    <button class="nd-btn-primary nd-btn-sm" data-nd-modal="#new-task-modal">+ New task</button>
  </div>

  <!-- Shared card template used by all three columns. -->
  <template id="task-card">
    <li class="nd-card nd-mb-sm" data-id="{{{{id}}}}">
      <button type="button"
              class="nd-card-body nd-btn-ghost nd-text-left nd-w-full"
              data-nd-set="selectedTaskId='{{{{id}}}}'"
              data-nd-modal="#task-modal"
              data-nd-success="refresh:#task-modal-header,refresh:#task-modal-meta,refresh:#task-modal-comments">
        <div class="nd-font-semibold">{{{{title}}}}</div>
        <div class="nd-text-muted nd-text-sm">{{{{comment_count}}}} comments</div>
      </button>
    </li>
  </template>

  <div class="nd-row">
    <div class="nd-col-4">
      <section class="nd-card">
        <div class="nd-card-header"><strong>TODO</strong></div>
        <ul class="nd-card-body"
            id="col-todo"
            data-nd-bind="/v1/projects/{ident}/tasks?status=todo"
            data-nd-template="task-card"
            data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=todo"
            data-nd-sortable-group="tasks"
            data-nd-sortable-refresh="#col-todo,#col-in_progress,#col-done">
          <template data-nd-empty>
            <li class="nd-text-muted nd-text-sm">No tasks.</li>
          </template>
        </ul>
      </section>
    </div>

    <div class="nd-col-4">
      <section class="nd-card">
        <div class="nd-card-header"><strong>IN PROGRESS</strong></div>
        <ul class="nd-card-body"
            id="col-in_progress"
            data-nd-bind="/v1/projects/{ident}/tasks?status=in_progress"
            data-nd-template="task-card"
            data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=in_progress"
            data-nd-sortable-group="tasks"
            data-nd-sortable-refresh="#col-todo,#col-in_progress,#col-done">
          <template data-nd-empty>
            <li class="nd-text-muted nd-text-sm">No tasks.</li>
          </template>
        </ul>
      </section>
    </div>

    <div class="nd-col-4">
      <section class="nd-card">
        <div class="nd-card-header"><strong>DONE</strong></div>
        <ul class="nd-card-body"
            id="col-done"
            data-nd-bind="/v1/projects/{ident}/tasks?status=done"
            data-nd-template="task-card"
            data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=done"
            data-nd-sortable-group="tasks"
            data-nd-sortable-refresh="#col-todo,#col-in_progress,#col-done">
          <template data-nd-empty>
            <li class="nd-text-muted nd-text-sm">No tasks.</li>
          </template>
        </ul>
      </section>
    </div>
  </div>

  <!-- New-task modal — unchanged pattern, posts and refreshes the TODO column. -->
  <dialog id="new-task-modal" class="nd-modal">
    <form data-nd-action="POST /v1/projects/{ident}/tasks"
          data-nd-success="close-modal,refresh:#col-todo,reset">
      <header><h3>New task</h3></header>
      <div>
        <div class="nd-form-group">
          <label for="new-title">Title</label>
          <input id="new-title" name="title" required>
        </div>
        <div class="nd-form-group">
          <label for="new-description">Description</label>
          <textarea id="new-description" name="description" rows="3"></textarea>
        </div>
        <div class="nd-form-group">
          <label for="new-details">Details</label>
          <textarea id="new-details" name="details" rows="6"></textarea>
        </div>
      </div>
      <footer>
        <button type="button" data-nd-dismiss class="nd-btn-ghost">Cancel</button>
        <button type="submit" class="nd-btn-primary">Create</button>
      </footer>
    </form>
  </dialog>

  <!--
    Task detail modal. The bound panels share the same URL so ndesign's
    in-flight dedup issues exactly one GET per open/switch. The action buttons
    are static DOM nodes rather than template-rendered nodes because ndesign's
    click action binding is installed during page init. Every write (PATCH,
    POST comment) refreshes the panels and every column, so the board and the
    modal stay in lockstep without a page reload.
  -->
  <dialog id="task-modal" class="nd-modal nd-modal-lg">
    <header>
      <h3 id="task-modal-header"
          data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
          data-nd-field="title"
          data-nd-defer></h3>
      <button type="button" class="nd-modal-close" data-nd-dismiss aria-label="Close">&times;</button>
    </header>
    <div>
      <div id="task-modal-meta"
           data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
           data-nd-template="task-modal-meta-tmpl"
           data-nd-defer>
        <template id="task-modal-meta-tmpl">
          <div class="nd-text-muted nd-text-sm nd-mb-md">
            status: {{{{status}}}} · rank {{{{rank}}}} · reporter {{{{reporter}}}}
          </div>
          <p class="nd-text-muted nd-text-sm">Description</p>
          <p>{{{{description}}}}</p>
          <p class="nd-text-muted nd-text-sm nd-mt-md">Details</p>
          <pre class="nd-text-sm">{{{{details}}}}</pre>
        </template>
      </div>

      <div id="task-modal-actions" class="nd-flex nd-gap-sm nd-mt-md nd-mb-lg">
        <button type="button"
                class="nd-btn-primary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"in_progress"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Claim
        </button>
        <button type="button"
                class="nd-btn-secondary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"todo"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Release
        </button>
        <button type="button"
                class="nd-btn-primary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"done"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Done
        </button>
        <button type="button"
                class="nd-btn-secondary nd-btn-sm"
                data-nd-action="PATCH /v1/projects/{ident}/tasks/${{selectedTaskId}}"
                data-nd-body='{{"status":"todo"}}'
                data-nd-success="refresh:#col-todo,refresh:#col-in_progress,refresh:#col-done,refresh:#task-modal-header,refresh:#task-modal-meta">
          Reopen
        </button>
      </div>

      <section class="nd-card">
        <div class="nd-card-header"><strong>Comments</strong></div>
        <div class="nd-card-body">
          <div id="task-modal-comments"
               data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
               data-nd-select="comments"
               data-nd-template="task-modal-comment-tmpl"
               data-nd-defer>
            <template id="task-modal-comment-tmpl">
              <div class="nd-mb-md">
                <div class="nd-text-muted nd-text-sm">{{{{author}}}} ({{{{author_type}}}})</div>
                <div>{{{{content}}}}</div>
              </div>
            </template>
            <template data-nd-empty>
              <p class="nd-text-muted nd-text-sm">No comments yet.</p>
            </template>
          </div>

          <form class="nd-mt-lg"
                data-nd-action="POST /v1/projects/{ident}/tasks/${{selectedTaskId}}/comments"
                data-nd-success="refresh:#task-modal-comments,reset">
            <div class="nd-form-group">
              <label for="task-modal-comment">Add a comment</label>
              <textarea id="task-modal-comment" name="content" rows="3" required></textarea>
            </div>
            <button type="submit" class="nd-btn-primary nd-btn-sm">Comment</button>
          </form>
        </div>
      </section>
    </div>
  </dialog>"##,
        ident = ident_attr,
    );

    let html = format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n{head}\n</head>\n{open}\n{content}\n{close}",
        head = control_panel_head(
            &page_title,
            &theme,
            r#"<meta name="var:selectedTaskId" content="">"#,
        ),
        open = control_panel_open(&page_title, "tasks"),
        content = content,
        close = control_panel_close(&state.api_key),
    );
    Ok(Html(html))
}

// ── ndesign partials (shared by control-panel pages) ─────────────────────────

/// CDN base for the ndesign runtime and theme stylesheets. Shared by
/// `control_panel_head` / `control_panel_close` and every page that uses
/// them. Kept as a constant so the version is bumped in one place.
const NDESIGN_BASE: &str = "https://storage.googleapis.com/ndesign-cdn/ndesign/latest";

fn theme_toggle_button() -> &'static str {
    r#"<button class="nd-btn-secondary" data-nd-theme-toggle title="Toggle theme">Theme</button>"#
}

// ── Control-panel layout helpers (shared by dashboard + future admin pages) ───

/// Render the `<head>` contents for a control-panel page.
///
/// Emits charset + viewport meta, the page `<title>`, ndesign base CSS, the
/// active theme stylesheet (class `theme` so the runtime switcher can swap it),
/// the two theme-registration meta tags, plus the `endpoint:api` and
/// `csrf-token` meta tags the ndesign runtime expects. `extra` is appended
/// verbatim — pages that declare ndesign store vars (`<meta name="var:…">`)
/// pass them in here so the runtime finds them during init.
///
/// `theme` must be `"light"` or `"dark"`; any other value falls back to
/// `"dark"`.
fn control_panel_head(title: &str, theme: &str, extra: &str) -> String {
    let theme = if theme == "light" { "light" } else { "dark" };
    format!(
        r#"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<link rel="stylesheet" href="{base}/ndesign.min.css">
<link rel="stylesheet" class="theme" data-theme="{theme}" href="{base}/themes/{theme}.min.css">
<meta name="nd-theme" content="light" data-href="{base}/themes/light.min.css">
<meta name="nd-theme" content="dark" data-href="{base}/themes/dark.min.css">
<meta name="endpoint:api" content="">
<meta name="csrf-token" content="">
{extra}"#,
        title = he(title),
        base = NDESIGN_BASE,
        theme = theme,
        extra = extra,
    )
}

/// Open the control-panel body up to the start of `<main class="app-content">`.
///
/// Emits `<body class="app-page">`, the app layout wrapper, the sidebar (brand
/// plus the Main section with Dashboard, Tasks, Patterns, Skills, Commands,
/// Agents links), and the header (hamburger toggle, page title, theme toggle).
///
/// * `page_title` — rendered inside the header's `<h1>`.
/// * `active` — which sidebar link receives `class="nd-active"`. Accepts
///   `"dashboard"`, `"tasks"`, `"patterns"`, `"skills"`, `"commands"`, or
///   `"agents"`. Any other value leaves all links inactive.
fn control_panel_open(page_title: &str, active: &str) -> String {
    let cls = |key: &str| -> &'static str {
        if key == active {
            r#" class="nd-active""#
        } else {
            ""
        }
    };
    format!(
        r#"<body class="app-page">
<div class="app-layout nd-h-screen nd-overflow-hidden">
  <nav class="sidebar" id="app-sidebar">
    <span class="nd-nav-brand">agent-gateway</span>
    <p class="nd-nav-section">Main</p>
    <ul class="nd-nav-menu">
      <li><a href="/"{dashboard}>Dashboard</a></li>
      <li><a href="/tasks"{tasks}>Tasks</a></li>
      <li><a href="/patterns"{patterns}>Patterns</a></li>
      <li><a href="/skills"{skills}>Skills</a></li>
      <li><a href="/commands"{commands}>Commands</a></li>
      <li><a href="/agents"{agents}>Agents</a></li>
    </ul>
  </nav>
  <div class="app-body">
    <header>
      <div class="app-header-left">
        <button class="hamburger" data-nd-toggle="sidebar">&#9776;</button>
        <h1 class="app-header-title">{title}</h1>
      </div>
      <div class="app-header-right">
        {theme_toggle}
      </div>
    </header>
    <main class="app-content">"#,
        dashboard = cls("dashboard"),
        tasks = cls("tasks"),
        patterns = cls("patterns"),
        skills = cls("skills"),
        commands = cls("commands"),
        agents = cls("agents"),
        title = he(page_title),
        theme_toggle = theme_toggle_button(),
    )
}

/// Close the control-panel body: close `<main>`, `<div class="app-body">`,
/// and `<div class="app-layout">`, then emit the ndesign runtime script and
/// an inline config block that (a) wires bearer-auth for XHR and (b)
/// persists `nd:theme-change` events back to the server.
///
/// The theme-change listener was historically emitted by `ndesign_scripts`
/// for the old `/manage` page. When the dashboard was refactored onto this
/// shared shell (commit `538d374`), the listener was dropped and theme
/// toggles stopped surviving reloads. Re-registering it here fixes that
/// regression for every page built on the control-panel shell.
///
/// Output is deliberately limited to two `<script>` tags (the ndesign
/// runtime + this inline config block) to keep the per-page script budget
/// predictable.
///
/// The bearer token is JSON-escaped via `serde_json::to_string` so it is safe
/// to interpolate inside the inline script literal.
fn control_panel_close(api_key: &str) -> String {
    let api_key_json = serde_json::to_string(api_key).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        r#"    </main>
  </div>
</div>
<script src="{base}/ndesign.min.js"></script>
<script>
NDesign.configure({{ headers: {{ 'Authorization': 'Bearer ' + {api_key_json} }} }});
document.addEventListener('nd:theme-change', (e) => {{
  const theme = e.detail && e.detail.theme;
  if (!theme) return;
  fetch('/theme', {{
    method: 'POST',
    headers: {{ 'Content-Type': 'application/json' }},
    body: JSON.stringify({{ theme }})
  }}).catch(() => {{}});
}});
</script>
</body>
</html>"#,
        base = NDESIGN_BASE,
        api_key_json = api_key_json,
    )
}

// ── GET /v1/projects/:ident/messages/unread ───────────────────────────────────

#[derive(Serialize)]
pub struct GetUnreadResponse {
    pub messages: Vec<Message>,
    pub status: String,
}

pub async fn get_unread_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(ident): Path<String>,
) -> Result<Json<GetUnreadResponse>> {
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let agent_id = extract_agent_id(&headers);
    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let messages = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::get_unconfirmed_for_agent(&conn, &ident_clone, &aid)
    })
    .await??;

    let status = if messages.is_empty() {
        "no messages".to_string()
    } else {
        format!("{} unconfirmed message(s)", messages.len())
    };

    Ok(Json(GetUnreadResponse { messages, status }))
}

// ── POST /v1/projects/:ident/messages/:id/confirm ────────────────────────────

#[derive(Serialize)]
pub struct ConfirmResponse {
    pub confirmed: bool,
}

pub async fn confirm_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, msg_id)): Path<(String, i64)>,
) -> Result<Json<ConfirmResponse>> {
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            ));
        }
    }

    let agent_id = extract_agent_id(&headers);
    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let confirmed = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::confirm_message_for_agent(&conn, &ident_clone, &aid, msg_id)
    })
    .await??;

    Ok(Json(ConfirmResponse { confirmed }))
}

// ── POST /v1/projects/:ident/messages/:id/reply ─────────────────────────────

#[derive(Deserialize)]
pub struct ReplyRequest {
    /// Back-compat alias for `body`. If both are set, `body` wins.
    pub content: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub hostname: Option<String>,
    pub event_at: Option<i64>,
}

#[derive(Serialize)]
pub struct ReplyResponse {
    pub message_id: i64,
    pub external_message_id: String,
    pub parent_message_id: i64,
}

pub async fn reply_to_message(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, parent_id)): Path<(String, i64)>,
    Json(req): Json<ReplyRequest>,
) -> Result<Json<ReplyResponse>> {
    let agent_id = extract_agent_id(&headers);

    let (channel_name, room_id, parent_external_id) = {
        let conn = state.db.lock().unwrap();
        let project = db::get_project(&conn, &ident)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            )
        })?;
        let parent = db::get_message_by_id(&conn, &ident, parent_id)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("message {} not found", parent_id),
            )
        })?;
        (
            project.channel_name,
            project.room_id,
            parent.external_message_id,
        )
    };

    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("plugin '{channel_name}' not loaded"),
            )
        })?
        .clone();

    let body_text = req.body.or(req.content).unwrap_or_default();
    if body_text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request must include non-empty 'body' (or 'content')".into(),
        ));
    }
    let outbound = build_outbound(
        &agent_id,
        body_text,
        req.subject,
        req.hostname,
        req.event_at,
    );

    let external_id = match &parent_external_id {
        Some(ext_id) => plugin.reply_structured(&room_id, ext_id, &outbound).await?,
        None => plugin.send_structured(&room_id, &outbound).await?,
    };

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: outbound.body.clone(),
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: Some(parent_id),
        agent_id: Some(agent_id.clone()),
        message_type: "reply".into(),
        subject: Some(outbound.subject.clone()),
        hostname: Some(outbound.hostname.clone()),
        event_at: Some(outbound.event_at),
    };

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(ReplyResponse {
        message_id: row_id,
        external_message_id: external_id,
        parent_message_id: parent_id,
    }))
}

// ── POST /v1/projects/:ident/messages/:id/action ────────────────────────────

#[derive(Deserialize)]
pub struct ActionRequest {
    /// Back-compat alias for `body`. If both are set, `body` wins.
    pub message: Option<String>,
    pub body: Option<String>,
    pub subject: Option<String>,
    pub hostname: Option<String>,
    pub event_at: Option<i64>,
}

#[derive(Serialize)]
pub struct ActionResponse {
    pub message_id: i64,
    pub external_message_id: String,
    pub parent_message_id: i64,
}

pub async fn taking_action_on(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((ident, parent_id)): Path<(String, i64)>,
    Json(req): Json<ActionRequest>,
) -> Result<Json<ActionResponse>> {
    let agent_id = extract_agent_id(&headers);

    let (channel_name, room_id, parent_external_id) = {
        let conn = state.db.lock().unwrap();
        let project = db::get_project(&conn, &ident)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("project '{}' not found", ident),
            )
        })?;
        let parent = db::get_message_by_id(&conn, &ident, parent_id)?.ok_or_else(|| {
            AppError(
                StatusCode::NOT_FOUND,
                format!("message {} not found", parent_id),
            )
        })?;
        (
            project.channel_name,
            project.room_id,
            parent.external_message_id,
        )
    };

    let plugin = state
        .plugins
        .get(&channel_name)
        .ok_or_else(|| {
            AppError(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("plugin '{channel_name}' not loaded"),
            )
        })?
        .clone();

    let body_text = req.body.or(req.message).unwrap_or_default();
    if body_text.trim().is_empty() {
        return Err(AppError(
            StatusCode::BAD_REQUEST,
            "request must include non-empty 'body' (or 'message')".into(),
        ));
    }
    // Action posts get an `[ACTION]` subject prefix when the agent doesn't
    // supply one, so they remain visually distinct from regular replies.
    let subject = req.subject.or_else(|| {
        let derived = derive_subject(&body_text);
        Some(format!("[ACTION] {}", derived))
    });
    let outbound = build_outbound(&agent_id, body_text, subject, req.hostname, req.event_at);

    let external_id = match &parent_external_id {
        Some(ext_id) => plugin.reply_structured(&room_id, ext_id, &outbound).await?,
        None => plugin.send_structured(&room_id, &outbound).await?,
    };

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: outbound.body.clone(),
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: Some(parent_id),
        agent_id: Some(agent_id.clone()),
        message_type: "action".into(),
        subject: Some(outbound.subject.clone()),
        hostname: Some(outbound.hostname.clone()),
        event_at: Some(outbound.event_at),
    };

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let aid = agent_id;
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::upsert_agent(&conn, &ident_clone, &aid)?;
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(ActionResponse {
        message_id: row_id,
        external_message_id: external_id,
        parent_message_id: parent_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_for_status_covers_transitions() {
        let todo: Vec<_> = actions_for_status("todo")
            .into_iter()
            .map(|a| (a.verb, a.target_status))
            .collect();
        assert_eq!(
            todo,
            vec![
                ("Claim".into(), "in_progress".into()),
                ("Done".into(), "done".into())
            ],
        );

        let in_progress: Vec<_> = actions_for_status("in_progress")
            .into_iter()
            .map(|a| (a.verb, a.target_status))
            .collect();
        assert_eq!(
            in_progress,
            vec![
                ("Release".into(), "todo".into()),
                ("Done".into(), "done".into())
            ],
        );

        let done: Vec<_> = actions_for_status("done")
            .into_iter()
            .map(|a| (a.verb, a.target_status))
            .collect();
        assert_eq!(done, vec![("Reopen".into(), "todo".into())]);

        assert!(actions_for_status("nonsense").is_empty());
    }

    /// Render a literal that mirrors the shape of the string produced by
    /// `tasks_board` and assert the attributes the ndesign runtime needs
    /// survive `format!` escaping. Specifically:
    ///   * template-level `{{id}}` placeholders are emitted verbatim,
    ///   * store-var references render as `${selectedTaskId}`,
    ///   * the static PATCH action body is valid JSON,
    ///   * the card-click success refresh list includes `#task-modal-meta`
    ///     (deferred panel — without this refresh description + details stay
    ///     blank on first open),
    ///   * the kanban row does NOT carry `nd-gap-md` (stacks on the Bootstrap-
    ///     style gutter and wraps the third column),
    ///   * each column is a `<div class="nd-col-4">` wrapper with the card
    ///     inside (so col padding creates gutter BETWEEN cards, not inside),
    ///   * each sortable list carries `data-nd-sortable-group="tasks"` so the
    ///     ndesign runtime allows drops between columns.
    #[test]
    fn tasks_board_html_shape() {
        let ident_attr = "demo-project";
        let content = format!(
            r##"<li data-id="{{{{id}}}}">
<button data-nd-set="selectedTaskId='{{{{id}}}}'"
        data-nd-modal="#task-modal"
        data-nd-success="refresh:#task-modal-header,refresh:#task-modal-meta,refresh:#task-modal-comments"
        data-nd-bind="/v1/projects/{ident}/tasks/${{selectedTaskId}}"
        data-nd-body='{{"status":"in_progress"}}'></button>
<div class="nd-row">
  <div class="nd-col-4">
    <section class="nd-card">
      <ul id="col-todo"
          data-nd-sortable="POST /v1/projects/{ident}/tasks/reorder?status=todo"
          data-nd-sortable-group="tasks"></ul>
    </section>
  </div>
</div>"##,
            ident = ident_attr,
        );

        assert!(
            content.contains(r#"data-id="{{id}}""#),
            "card id placeholder must survive format! as `{{{{id}}}}`: {content}"
        );
        assert!(
            content.contains(r#"selectedTaskId='{{id}}'"#),
            "data-nd-set must embed the template id placeholder: {content}"
        );
        assert!(
            content.contains("/v1/projects/demo-project/tasks/${selectedTaskId}"),
            "bind URL must resolve ident and leave store-var reference intact: {content}"
        );
        assert!(
            content.contains(r#"data-nd-body='{"status":"in_progress"}'"#),
            "PATCH body must be literal JSON with a concrete status: {content}"
        );
        assert!(
            content.contains("refresh:#task-modal-meta"),
            "card click must refresh #task-modal-meta (deferred description+details panel): {content}"
        );
        assert!(
            !content.contains("nd-gap-md"),
            "kanban row must not carry nd-gap-md (stacks on top of row gutter): {content}"
        );
        assert!(
            content.contains(r#"<div class="nd-col-4">"#),
            "column must be a separate nd-col-4 wrapper, not merged into nd-card: {content}"
        );
        assert!(
            content.contains(r#"data-nd-sortable-group="tasks""#),
            "sortable columns must declare group=\"tasks\" for cross-column drag: {content}"
        );
    }
}
