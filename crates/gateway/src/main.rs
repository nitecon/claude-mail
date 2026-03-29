mod db;
mod discord;
mod projects;
mod routes;

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::task::spawn_blocking;
use tracing::info;

use db::Db;
use discord::DiscordHttp;

// ── App state shared across all handlers ─────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub discord: Arc<DiscordHttp>,
    pub project_channel_ids: Arc<Mutex<std::collections::HashSet<u64>>>,
    pub api_key: String,
}

// ── Auth middleware ───────────────────────────────────────────────────────────

async fn bearer_auth(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    if token.map(|t| t == state.api_key).unwrap_or(false) {
        next.run(request).await
    } else {
        StatusCode::UNAUTHORIZED.into_response()
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env (best-effort)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("RUST_LOG")
                .add_directive("gateway=info".parse()?),
        )
        .init();

    // ── Config ───────────────────────────────────────────────────────────────
    let discord_token =
        require_env("DISCORD_BOT_TOKEN");
    let guild_id: u64 = require_env("DISCORD_GUILD_ID")
        .parse()
        .context("DISCORD_GUILD_ID must be a u64")?;
    let category_id: Option<u64> = std::env::var("DISCORD_CATEGORY_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().context("DISCORD_CATEGORY_ID must be a u64"))
        .transpose()?;
    let api_key = require_env("GATEWAY_API_KEY");
    let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = std::env::var("GATEWAY_PORT")
        .unwrap_or_else(|_| "3000".into())
        .parse()
        .context("GATEWAY_PORT must be a u16")?;
    let db_path =
        std::env::var("DATABASE_PATH").unwrap_or_else(|_| "./data/claude-mail.db".into());
    let retention_days: u64 = std::env::var("MESSAGE_RETENTION_DAYS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .context("MESSAGE_RETENTION_DAYS must be a u64")?;

    // ── Database ─────────────────────────────────────────────────────────────
    // Ensure parent directory exists.
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).context("create db directory")?;
    }
    let db = db::open(&db_path)?;
    info!("SQLite database opened at {db_path}");

    // ── Discord ───────────────────────────────────────────────────────────────
    let project_channel_ids: Arc<Mutex<std::collections::HashSet<u64>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));

    let discord = discord::start(
        &discord_token,
        guild_id,
        category_id,
        db.clone(),
        project_channel_ids.clone(),
    )
    .await?;
    info!("Discord bot started (guild={guild_id})");

    // ── App state ─────────────────────────────────────────────────────────────
    let state = AppState {
        db: db.clone(),
        discord,
        project_channel_ids,
        api_key,
    };

    // ── Retention task ────────────────────────────────────────────────────────
    {
        let db = db.clone();
        let retention_ms = retention_days as i64 * 24 * 60 * 60 * 1000;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(86_400));
            loop {
                interval.tick().await;
                let db = db.clone();
                let cutoff = db::now_ms() - retention_ms;
                let result = spawn_blocking(move || {
                    let conn = db.lock().unwrap();
                    db::purge_old_messages(&conn, cutoff)
                })
                .await;
                match result {
                    Ok(Ok(n)) => info!("Purged {n} old messages"),
                    Ok(Err(e)) => tracing::warn!("Purge error: {e}"),
                    Err(e) => tracing::warn!("Purge task error: {e}"),
                }
            }
        });
    }

    // ── Router ────────────────────────────────────────────────────────────────
    let app = Router::new()
        .route("/v1/projects", post(routes::register_project))
        .route("/v1/projects/{ident}/messages", post(routes::send_message))
        .route(
            "/v1/projects/{ident}/messages/unread",
            get(routes::get_unread_messages),
        )
        .layer(middleware::from_fn_with_state(state.clone(), bearer_auth))
        .with_state(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .context("parse listen address")?;

    info!("Gateway listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

fn require_env(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| {
        eprintln!("Missing required environment variable: {key}");
        std::process::exit(1);
    })
}
