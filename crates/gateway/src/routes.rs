use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use tracing::error;

use crate::{
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
        AppError(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

type Result<T> = std::result::Result<T, AppError>;

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
    pub content: String,
}

#[derive(Serialize)]
pub struct SendMessageResponse {
    pub message_id: i64,
    pub external_message_id: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Json(body): Json<SendMessageRequest>,
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

    let formatted = format!("[AGENT] {}", body.content);
    let external_id = plugin.send(&room_id, &formatted).await?;

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: body.content,
        sent_at: now_ms(),
    };

    let db = state.db.clone();
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
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
    pub size: i64,
    pub checksum: String,
}

pub async fn upload_skill(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Bytes,
) -> Result<Json<SkillUploadResponse>> {
    use sha2::{Digest, Sha256};
    let zip = body.to_vec();
    let size = zip.len() as i64;
    let mut hasher = Sha256::new();
    hasher.update(&zip);
    let checksum = hex::encode(hasher.finalize());

    let record = db::SkillRecord {
        name: name.clone(),
        zip_data: zip,
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
        size,
        checksum,
    }))
}

pub async fn list_skills_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<db::SkillMeta>>> {
    let db = state.db.clone();
    let skills = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::list_skills(&conn)
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
        None => Err(AppError(StatusCode::NOT_FOUND, "skill not found".into())),
        Some(r) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/zip"),
            );
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

// ── GET / (dashboard) ─────────────────────────────────────────────────────────

fn he(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn dashboard(State(state): State<AppState>) -> Result<Html<String>> {
    let db = state.db.clone();
    let data = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_dashboard_data(&conn)
    })
    .await??;

    let rows = data
        .projects
        .iter()
        .map(|p| {
            let unread_cell = if p.unread_count > 0 {
                format!(
                    "<span style='color:#e53e3e;font-weight:600'>{}</span>",
                    p.unread_count
                )
            } else {
                "0".into()
            };
            format!(
                "<tr><td>{}</td><td>{}</td><td class='muted'>{}</td><td>{}</td><td>{}</td></tr>",
                he(&p.ident),
                he(&p.channel_name),
                he(&p.room_id),
                p.total_messages,
                unread_cell,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let empty_row = if data.project_count == 0 {
        "<tr><td colspan='5' style='text-align:center;color:#a0aec0;padding:2rem'>No projects registered yet</td></tr>"
    } else {
        ""
    };

    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>claude-mail Gateway</title>
<style>
  *{{box-sizing:border-box;margin:0;padding:0}}
  body{{font-family:system-ui,sans-serif;background:#f7fafc;color:#1a202c;padding:2rem 1rem}}
  .wrap{{max-width:960px;margin:0 auto}}
  header{{margin-bottom:1.5rem}}
  h1{{font-size:1.4rem;font-weight:700}}
  .sub{{color:#718096;font-size:0.9rem;margin-top:0.2rem}}
  .stats{{display:flex;flex-wrap:wrap;gap:1rem;margin:1.5rem 0}}
  .card{{background:#fff;border:1px solid #e2e8f0;border-radius:8px;padding:1rem 1.5rem;min-width:140px}}
  .card-n{{font-size:2rem;font-weight:700;line-height:1.1}}
  .card-l{{font-size:0.75rem;color:#718096;text-transform:uppercase;letter-spacing:0.05em;margin-top:0.3rem}}
  .section{{background:#fff;border:1px solid #e2e8f0;border-radius:8px;overflow:hidden}}
  .section-head{{padding:0.75rem 1rem;border-bottom:1px solid #e2e8f0;font-size:0.8rem;font-weight:600;color:#4a5568;text-transform:uppercase;letter-spacing:0.05em}}
  table{{width:100%;border-collapse:collapse}}
  th{{text-align:left;padding:0.6rem 1rem;font-size:0.78rem;font-weight:600;color:#4a5568;text-transform:uppercase;letter-spacing:0.04em;background:#f7fafc;border-bottom:1px solid #e2e8f0}}
  td{{padding:0.65rem 1rem;font-size:0.88rem;border-bottom:1px solid #edf2f7}}
  tr:last-child td{{border-bottom:none}}
  tr:hover td{{background:#f7fafc}}
  .muted{{color:#718096;font-size:0.8rem}}
</style></head>
<body><div class="wrap">
<header>
  <h1>claude-mail Gateway</h1>
  <div class="sub">Channel plugin dashboard</div>
</header>
<div class="stats">
  <div class="card"><div class="card-n">{}</div><div class="card-l">Projects</div></div>
  <div class="card"><div class="card-n">{}</div><div class="card-l">Total messages</div></div>
  <div class="card"><div class="card-n">{}</div><div class="card-l">Agent</div></div>
  <div class="card"><div class="card-n">{}</div><div class="card-l">User</div></div>
  <div class="card"><div class="card-n">{}</div><div class="card-l">Skills</div></div>
</div>
<div class="section">
  <div class="section-head">Projects</div>
  <table>
  <thead><tr>
    <th>Project</th><th>Channel</th><th>Room ID</th><th>Messages</th><th>Unread</th>
  </tr></thead>
  <tbody>{}{}</tbody>
  </table>
</div>
</div></body></html>"#,
        data.project_count,
        data.total_messages,
        data.agent_messages,
        data.user_messages,
        data.skill_count,
        rows,
        empty_row,
    );

    Ok(Html(html))
}

// ── GET /v1/projects/:ident/messages/unread ───────────────────────────────────

#[derive(Serialize)]
pub struct GetUnreadResponse {
    pub messages: Vec<Message>,
    pub status: String,
}

pub async fn get_unread_messages(
    State(state): State<AppState>,
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

    let db = state.db.clone();
    let ident_clone = ident.clone();
    let messages = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::get_and_advance_cursor(&conn, &ident_clone)
    })
    .await??;

    let status = if messages.is_empty() {
        "no messages".to_string()
    } else {
        format!("{} message(s)", messages.len())
    };

    Ok(Json(GetUnreadResponse { messages, status }))
}
