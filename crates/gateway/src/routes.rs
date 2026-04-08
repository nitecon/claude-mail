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
    headers: HeaderMap,
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

    let agent_id = extract_agent_id(&headers);
    let formatted = if agent_id == "_default" {
        format!("[AGENT] {}", body.content)
    } else {
        format!("[AGENT:{}] {}", agent_id, body.content)
    };
    let external_id = plugin.send(&room_id, &formatted).await?;

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: body.content,
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: None,
        agent_id: Some(agent_id.clone()),
        message_type: "message".into(),
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

// ── GET /manage ──────────────────────────────────────────────────────────────

pub async fn manage_page(State(state): State<AppState>) -> Html<String> {
    let api_key = he(&state.api_key);
    Html(format!(
        r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width">
<title>agent-gateway — Manage</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:system-ui,sans-serif;background:#f7fafc;color:#1a202c;padding:2rem 1rem}}
.wrap{{max-width:1060px;margin:0 auto}}
header{{margin-bottom:1.5rem;display:flex;justify-content:space-between;align-items:center}}
h1{{font-size:1.4rem;font-weight:700}}
.sub{{color:#718096;font-size:0.9rem;margin-top:0.2rem}}
a.back{{color:#4a5568;text-decoration:none;font-size:0.85rem}}
a.back:hover{{text-decoration:underline}}
.tabs{{display:flex;gap:0;margin-bottom:1rem}}
.tab{{padding:0.5rem 1.2rem;cursor:pointer;border:1px solid #e2e8f0;background:#fff;font-size:0.85rem;font-weight:600;color:#4a5568;transition:all 0.15s}}
.tab:first-child{{border-radius:6px 0 0 6px}}
.tab:last-child{{border-radius:0 6px 6px 0}}
.tab.active{{background:#4a5568;color:#fff;border-color:#4a5568}}
.section{{background:#fff;border:1px solid #e2e8f0;border-radius:8px;overflow:hidden}}
.section-head{{padding:0.75rem 1rem;border-bottom:1px solid #e2e8f0;display:flex;justify-content:space-between;align-items:center}}
.section-head span{{font-size:0.8rem;font-weight:600;color:#4a5568;text-transform:uppercase;letter-spacing:0.05em}}
table{{width:100%;border-collapse:collapse}}
th{{text-align:left;padding:0.6rem 1rem;font-size:0.78rem;font-weight:600;color:#4a5568;text-transform:uppercase;background:#f7fafc;border-bottom:1px solid #e2e8f0}}
td{{padding:0.55rem 1rem;font-size:0.85rem;border-bottom:1px solid #edf2f7}}
tr:last-child td{{border-bottom:none}}
tr:hover td{{background:#f7fafc}}
.muted{{color:#718096;font-size:0.8rem}}
.btn{{padding:0.35rem 0.75rem;border:1px solid #e2e8f0;border-radius:4px;background:#fff;cursor:pointer;font-size:0.78rem;font-weight:500;transition:all 0.15s}}
.btn:hover{{background:#edf2f7}}
.btn-primary{{background:#4a5568;color:#fff;border-color:#4a5568}}
.btn-primary:hover{{background:#2d3748}}
.btn-danger{{color:#e53e3e;border-color:#fed7d7}}
.btn-danger:hover{{background:#fff5f5}}
.btn-sm{{padding:0.25rem 0.5rem;font-size:0.75rem}}
.actions{{display:flex;gap:0.4rem}}
.editor{{display:none;padding:1rem;border-top:1px solid #e2e8f0;background:#fafbfc}}
.editor.visible{{display:block}}
.editor textarea{{width:100%;min-height:300px;font-family:'SF Mono',Monaco,monospace;font-size:0.85rem;padding:0.75rem;border:1px solid #e2e8f0;border-radius:6px;resize:vertical;line-height:1.5}}
.editor-bar{{display:flex;justify-content:space-between;align-items:center;margin-top:0.75rem}}
.editor-title{{font-weight:600;font-size:0.9rem}}
.create-form{{display:none;padding:1rem;border-top:1px solid #e2e8f0;background:#fafbfc}}
.create-form.visible{{display:block}}
.form-row{{display:flex;gap:0.75rem;margin-bottom:0.75rem;align-items:center}}
.form-row label{{font-size:0.8rem;font-weight:600;color:#4a5568;min-width:60px}}
.form-row input[type=text]{{flex:1;padding:0.4rem 0.6rem;border:1px solid #e2e8f0;border-radius:4px;font-size:0.85rem}}
.form-row input[type=file]{{flex:1;font-size:0.85rem}}
.msg{{padding:0.5rem 1rem;margin-bottom:0.75rem;border-radius:4px;font-size:0.85rem;display:none}}
.msg.ok{{display:block;background:#f0fff4;color:#276749;border:1px solid #c6f6d5}}
.msg.err{{display:block;background:#fff5f5;color:#c53030;border:1px solid #fed7d7}}
</style></head>
<body><div class="wrap">
<header>
  <div><h1>agent-gateway — Manage</h1><div class="sub">Skills, Commands &amp; Agents</div></div>
  <a class="back" href="/">&larr; Dashboard</a>
</header>
<div id="msg" class="msg"></div>
<div class="tabs">
  <div class="tab active" onclick="setTab('command')">Commands</div>
  <div class="tab" onclick="setTab('agent')">Agents</div>
  <div class="tab" onclick="setTab('skill')">Skills</div>
</div>
<div class="section">
  <div class="section-head">
    <span id="section-title">Commands</span>
    <div class="actions">
      <button class="btn btn-primary btn-sm" onclick="toggleCreate()">+ New</button>
    </div>
  </div>
  <div id="create-form" class="create-form">
    <div class="form-row"><label>Name</label><input type="text" id="create-name" placeholder="my-item"></div>
    <div id="create-text-row" class="form-row" style="align-items:flex-start"><label>Content</label><textarea id="create-content" style="flex:1;min-height:150px;font-family:monospace;font-size:0.85rem;padding:0.5rem;border:1px solid #e2e8f0;border-radius:4px" placeholder="Markdown content..."></textarea></div>
    <div id="create-file-row" class="form-row" style="display:none"><label>Zip file</label><input type="file" id="create-file" accept=".zip"></div>
    <div class="form-row"><label></label><button class="btn btn-primary" onclick="doCreate()">Create</button><button class="btn" onclick="toggleCreate()" style="margin-left:0.5rem">Cancel</button></div>
  </div>
  <table>
    <thead><tr><th>Name</th><th>Size</th><th>Updated</th><th style="width:120px">Actions</th></tr></thead>
    <tbody id="list-body"><tr><td colspan="4" class="muted" style="text-align:center;padding:2rem">Loading...</td></tr></tbody>
  </table>
  <div id="editor" class="editor">
    <div class="editor-bar"><span class="editor-title" id="editor-title">Editing: —</span><button class="btn" onclick="closeEditor()">Close</button></div>
    <textarea id="editor-content" style="margin-top:0.75rem"></textarea>
    <div class="editor-bar"><span class="muted" id="editor-hint"></span><button class="btn btn-primary" onclick="doSave()">Save</button></div>
  </div>
</div>
<script>
const K='{api_key}';
const H={{'Authorization':'Bearer '+K}};
let tab='command', editName=null;

function setTab(t){{
  tab=t;
  document.querySelectorAll('.tab').forEach(el=>el.classList.remove('active'));
  document.querySelector('.tab[onclick*="\''+t+'\'"]').classList.add('active');
  document.getElementById('section-title').textContent=t==='command'?'Commands':t==='agent'?'Agents':'Skills';
  document.getElementById('create-text-row').style.display=t==='skill'?'none':'flex';
  document.getElementById('create-file-row').style.display=t==='skill'?'flex':'none';
  closeEditor();hideCreate();loadList();
}}

function showMsg(text,ok){{
  const el=document.getElementById('msg');
  el.textContent=text;el.className='msg '+(ok?'ok':'err');
  setTimeout(()=>{{el.className='msg'}},4000);
}}

function fmtDate(ms){{
  const d=new Date(ms);
  return d.getUTCFullYear()+'-'+String(d.getUTCMonth()+1).padStart(2,'0')+'-'+String(d.getUTCDate()).padStart(2,'0')+' '+String(d.getUTCHours()).padStart(2,'0')+':'+String(d.getUTCMinutes()).padStart(2,'0');
}}

function fmtSize(b){{
  if(b<1024)return b+' B';
  if(b<1048576)return (b/1024).toFixed(1)+' KB';
  return (b/1048576).toFixed(1)+' MB';
}}

async function loadList(){{
  const resp=await fetch('/v1/skills',{{headers:H}});
  if(!resp.ok){{showMsg('Failed to load list',false);return;}}
  const items=(await resp.json()).filter(s=>s.kind===tab);
  const tbody=document.getElementById('list-body');
  if(!items.length){{
    tbody.innerHTML='<tr><td colspan="4" class="muted" style="text-align:center;padding:2rem">No '+tab+'s yet</td></tr>';
    return;
  }}
  tbody.innerHTML=items.map(s=>`<tr>
    <td>${{s.name}}</td>
    <td class="muted">${{fmtSize(s.size)}}</td>
    <td class="muted">${{fmtDate(s.uploaded_at)}}</td>
    <td><div class="actions">
      ${{tab!=='skill'?`<button class="btn btn-sm" onclick="doEdit('${{s.name}}')">Edit</button>`:`<a class="btn btn-sm" href="/v1/skills/${{s.name}}" target="_blank">Download</a>`}}
      <button class="btn btn-sm btn-danger" onclick="doDelete('${{s.name}}')">Delete</button>
    </div></td>
  </tr>`).join('');
}}

async function doEdit(name){{
  editName=name;
  const resp=await fetch('/v1/skills/'+encodeURIComponent(name)+'/content',{{headers:H}});
  if(!resp.ok){{showMsg('Failed to load content',false);return;}}
  const data=await resp.json();
  if(data.content===null){{showMsg('Binary skill — download to edit',false);return;}}
  document.getElementById('editor-title').textContent='Editing: '+name;
  document.getElementById('editor-content').value=data.content;
  document.getElementById('editor-hint').textContent=data.kind;
  document.getElementById('editor').classList.add('visible');
}}

function closeEditor(){{
  editName=null;
  document.getElementById('editor').classList.remove('visible');
}}

async function doSave(){{
  if(!editName)return;
  const content=document.getElementById('editor-content').value;
  const resp=await fetch('/v1/skills/'+encodeURIComponent(editName),{{
    method:'PUT',headers:{{...H,'Content-Type':'text/markdown','X-Kind':tab}},body:content
  }});
  if(resp.ok){{showMsg('Saved '+editName,true);loadList();}}
  else{{const e=await resp.json().catch(()=>({{}}));showMsg(e.error||'Save failed',false);}}
}}

async function doDelete(name){{
  if(!confirm('Delete "'+name+'"?'))return;
  const resp=await fetch('/v1/skills/'+encodeURIComponent(name),{{method:'DELETE',headers:H}});
  if(resp.ok||resp.status===204){{showMsg('Deleted '+name,true);closeEditor();loadList();}}
  else{{showMsg('Delete failed',false);}}
}}

function toggleCreate(){{
  const el=document.getElementById('create-form');
  el.classList.toggle('visible');
  if(!el.classList.contains('visible')){{
    document.getElementById('create-name').value='';
    document.getElementById('create-content').value='';
  }}
}}
function hideCreate(){{document.getElementById('create-form').classList.remove('visible');}}

async function doCreate(){{
  const name=document.getElementById('create-name').value.trim();
  if(!name){{showMsg('Name is required',false);return;}}
  if(tab==='skill'){{
    const file=document.getElementById('create-file').files[0];
    if(!file){{showMsg('Select a zip file',false);return;}}
    const buf=await file.arrayBuffer();
    const resp=await fetch('/v1/skills/'+encodeURIComponent(name),{{
      method:'PUT',headers:{{...H,'Content-Type':'application/zip','X-Kind':'skill'}},body:buf
    }});
    if(resp.ok){{showMsg('Created skill '+name,true);hideCreate();loadList();}}
    else{{const e=await resp.json().catch(()=>({{}}));showMsg(e.error||'Create failed',false);}}
  }} else {{
    const content=document.getElementById('create-content').value;
    if(!content.trim()){{showMsg('Content is required',false);return;}}
    const resp=await fetch('/v1/skills/'+encodeURIComponent(name),{{
      method:'PUT',headers:{{...H,'Content-Type':'text/markdown','X-Kind':tab}},body:content
    }});
    if(resp.ok){{showMsg('Created '+tab+' '+name,true);hideCreate();loadList();}}
    else{{const e=await resp.json().catch(()=>({{}}));showMsg(e.error||'Create failed',false);}}
  }}
}}

loadList();
</script>
</div></body></html>"##,
        api_key = api_key,
    ))
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

    // Build the update banner if a newer version is available.
    let current_version = env!("CARGO_PKG_VERSION");
    let update_banner = {
        let guard = state.update_available.lock().unwrap();
        match guard.as_deref() {
            Some(version) => format!(
                r#"<div style="background:#fefcbf;border:1px solid #ecc94b;border-radius:8px;padding:0.75rem 1rem;margin-bottom:1.5rem;color:#744210;font-size:0.9rem">
  <strong>Update available:</strong> {} (current: v{})
  <div style="margin-top:0.3rem;font-size:0.82rem;color:#975a16">Run: <code style="background:#fefce8;padding:0.15rem 0.4rem;border-radius:4px">gateway update</code></div>
</div>"#,
                he(version),
                he(current_version),
            ),
            None => String::new(),
        }
    };

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
<title>agent-gateway Gateway</title>
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
{}
<header>
  <h1>agent-gateway Gateway</h1>
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
        update_banner,
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
    pub content: String,
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
    Json(body): Json<ReplyRequest>,
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

    let formatted = if agent_id == "_default" {
        format!("[AGENT] {}", body.content)
    } else {
        format!("[AGENT:{}] {}", agent_id, body.content)
    };

    let external_id = match &parent_external_id {
        Some(ext_id) => plugin.reply(&room_id, ext_id, &formatted).await?,
        None => plugin.send(&room_id, &formatted).await?,
    };

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: body.content,
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: Some(parent_id),
        agent_id: Some(agent_id.clone()),
        message_type: "reply".into(),
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
    pub message: String,
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
    Json(body): Json<ActionRequest>,
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

    let formatted = if agent_id == "_default" {
        format!("[ACTION] {}", body.message)
    } else {
        format!("[ACTION:{}] {}", agent_id, body.message)
    };

    let external_id = match &parent_external_id {
        Some(ext_id) => plugin.reply(&room_id, ext_id, &formatted).await?,
        None => plugin.send(&room_id, &formatted).await?,
    };

    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        external_message_id: Some(external_id.clone()),
        content: body.message,
        sent_at: now_ms(),
        confirmed_at: None,
        parent_message_id: Some(parent_id),
        agent_id: Some(agent_id.clone()),
        message_type: "action".into(),
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
