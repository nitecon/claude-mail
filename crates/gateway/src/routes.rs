use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use tokio::task::spawn_blocking;
use tracing::error;

use crate::{
    AppState,
    db::{self, Message, Project, now_ms},
    discord,
    projects::sanitize_ident,
};

// ── Error helper ─────────────────────────────────────────────────────────────

pub(crate) struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("handler error: {:?}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": self.0.to_string()})),
        )
            .into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

type Result<T> = std::result::Result<T, AppError>;

// ── POST /v1/projects ─────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RegisterProjectRequest {
    /// Raw project identity (git remote URL or directory name).
    pub ident: String,
}

#[derive(Serialize)]
pub struct RegisterProjectResponse {
    pub ident: String,
    pub channel_name: String,
    pub discord_channel_id: String,
}

pub async fn register_project(
    State(state): State<AppState>,
    Json(body): Json<RegisterProjectRequest>,
) -> Result<Json<RegisterProjectResponse>> {
    let channel_name = sanitize_ident(&body.ident);

    // Check if already registered.
    {
        let conn = state.db.lock().unwrap();
        if let Some(existing) = db::get_project(&conn, &channel_name)? {
            return Ok(Json(RegisterProjectResponse {
                ident: channel_name.clone(),
                channel_name: channel_name.clone(),
                discord_channel_id: existing.discord_channel_id,
            }));
        }
    }

    // Ensure Discord channel exists.
    let discord_channel_id = discord::ensure_channel(
        &state.discord,
        &channel_name,
        &state.project_channel_ids,
    )
    .await?;

    // Persist.
    let now = now_ms();
    let project = Project {
        ident: channel_name.clone(),
        discord_channel_id: discord_channel_id.clone(),
        last_discord_msg_id: None,
        created_at: now,
    };

    let db = state.db.clone();
    let project_clone = project.clone();
    spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_project(&conn, &project_clone)
    })
    .await??;

    Ok(Json(RegisterProjectResponse {
        ident: channel_name.clone(),
        channel_name,
        discord_channel_id,
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
    pub discord_message_id: String,
}

pub async fn send_message(
    State(state): State<AppState>,
    Path(ident): Path<String>,
    Json(body): Json<SendMessageRequest>,
) -> Result<Json<SendMessageResponse>> {
    // Verify project exists and get channel ID in one lock.
    let channel_id = {
        let conn = state.db.lock().unwrap();
        match db::get_project(&conn, &ident)? {
            Some(p) => p.discord_channel_id,
            None => return Err(AppError(anyhow::anyhow!("project '{}' not found", ident))),
        }
    };

    // Format and send to Discord.
    let formatted = format!("[AGENT] {}", body.content);
    let discord_id = discord::post_message(&state.discord, &channel_id, &formatted).await?;

    // Persist in local DB.
    let now = now_ms();
    let msg = Message {
        id: 0,
        project_ident: ident.clone(),
        source: "agent".into(),
        discord_message_id: Some(discord_id.clone()),
        content: body.content,
        sent_at: now,
    };

    let db = state.db.clone();
    let row_id = spawn_blocking(move || {
        let conn = db.lock().unwrap();
        db::insert_message(&conn, &msg)
    })
    .await??;

    Ok(Json(SendMessageResponse {
        message_id: row_id,
        discord_message_id: discord_id,
    }))
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
    // Verify project exists.
    {
        let conn = state.db.lock().unwrap();
        if db::get_project(&conn, &ident)?.is_none() {
            return Err(AppError(anyhow::anyhow!("project '{}' not found", ident)));
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
