mod channel;
mod channels;
mod db;
mod projects;
mod routes;

use anyhow::{Context, Result};
use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
    Router,
};
use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::spawn_blocking};
use tracing::info;

use channel::{ChannelPlugin, PluginEvent};
use db::Db;

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub plugins: Arc<HashMap<String, Arc<dyn ChannelPlugin>>>,
    pub default_channel: String,
    pub api_key: String,
}

// ── Auth middleware ───────────────────────────────────────────────────────────

async fn bearer_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
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

// ── Inbound message processor ─────────────────────────────────────────────────

fn spawn_inbound_processor(db: Db, mut rx: mpsc::Receiver<PluginEvent>) {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let PluginEvent::Message {
                channel_name,
                room_id,
                message,
            } = event;

            let db = db.clone();
            let result = spawn_blocking(move || {
                let conn = db.lock().unwrap();

                // Resolve room_id → project_ident.
                let project = match db::get_project_by_room(&conn, &channel_name, &room_id)? {
                    Some(p) => p,
                    None => {
                        tracing::warn!(
                            "Received message for unknown room {room_id} on {channel_name}"
                        );
                        return Ok::<_, anyhow::Error>(());
                    }
                };

                let m = db::Message {
                    id: 0,
                    project_ident: project.ident.clone(),
                    source: "user".into(),
                    external_message_id: Some(message.id.clone()),
                    content: message.content,
                    sent_at: db::now_ms(),
                    confirmed_at: None,
                };
                db::insert_message(&conn, &m)?;
                db::update_last_msg_id(&conn, &project.ident, &message.id)?;
                Ok(())
            })
            .await;

            if let Err(e) = result {
                tracing::error!("Inbound processor task error: {e}");
            } else if let Ok(Err(e)) = result {
                tracing::error!("Inbound processor db error: {e}");
            }
        }
    });
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("RUST_LOG")
                .add_directive("gateway=info".parse()?),
        )
        .init();

    // ── Background update check (non-blocking) ────────────────────────────────
    {
        let check_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        tokio::spawn(async move {
            match updater::check_update(&check_client, env!("CARGO_PKG_VERSION")).await {
                Ok(Some(v)) => tracing::warn!(
                    "A new version of claude-mail is available: {} (current: {}). \
                     Visit https://github.com/nitecon/claude-mail/releases",
                    v,
                    env!("CARGO_PKG_VERSION")
                ),
                Ok(None) => {}
                Err(e) => tracing::debug!("Update check failed: {e}"),
            }
        });
    }

    // ── Config ────────────────────────────────────────────────────────────────
    let api_key = require_env("GATEWAY_API_KEY");
    let default_channel = std::env::var("DEFAULT_CHANNEL").unwrap_or_else(|_| "discord".into());
    let host = std::env::var("GATEWAY_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let port: u16 = std::env::var("GATEWAY_PORT")
        .unwrap_or_else(|_| "7913".into())
        .parse()
        .context("GATEWAY_PORT must be a u16")?;
    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|h| {
                h.join(".claude")
                    .join("claude-mail")
                    .join("claude-mail.db")
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_else(|| "./data/claude-mail.db".into())
    });
    let retention_days: u64 = std::env::var("MESSAGE_RETENTION_DAYS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .context("MESSAGE_RETENTION_DAYS must be a u64")?;

    // ── Database ──────────────────────────────────────────────────────────────
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        std::fs::create_dir_all(parent).context("create db directory")?;
    }
    let db = db::open(&db_path)?;
    info!("SQLite database opened at {db_path}");

    // ── Plugin registry ───────────────────────────────────────────────────────
    let mut plugins: HashMap<String, Arc<dyn ChannelPlugin>> = HashMap::new();

    #[cfg(feature = "discord")]
    {
        let token = require_env("DISCORD_BOT_TOKEN");
        let guild_id: u64 = require_env("DISCORD_GUILD_ID")
            .parse()
            .context("DISCORD_GUILD_ID must be a u64")?;
        let category_id: Option<u64> = std::env::var("DISCORD_CATEGORY_ID")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| s.parse().context("DISCORD_CATEGORY_ID must be a u64"))
            .transpose()?;

        let discord = Arc::new(channels::discord::DiscordPlugin::new(
            channels::discord::DiscordConfig {
                token,
                guild_id,
                category_id,
            },
        ));
        plugins.insert("discord".into(), discord);
        info!("Registered channel plugin: discord");
    }

    if plugins.is_empty() {
        anyhow::bail!("No channel plugins enabled. Build with --features discord (or others).");
    }
    if !plugins.contains_key(&default_channel) {
        anyhow::bail!(
            "DEFAULT_CHANNEL='{default_channel}' is not among the enabled plugins: {:?}",
            plugins.keys().collect::<Vec<_>>()
        );
    }

    // ── Load existing projects and register rooms with their plugins ──────────
    let existing_projects = {
        let conn = db.lock().unwrap();
        db::all_projects(&conn)?
    };
    for project in &existing_projects {
        if let Some(plugin) = plugins.get(&project.channel_name) {
            plugin.register_room(&project.room_id, project.last_msg_id.as_deref());
        }
    }
    info!(
        "Registered {} existing project room(s)",
        existing_projects.len()
    );

    // ── Start plugins (connects gateways, spawns background tasks) ────────────
    let (tx, rx) = mpsc::channel::<PluginEvent>(256);
    for plugin in plugins.values() {
        plugin.start(tx.clone()).await?;
        info!("Started channel plugin: {}", plugin.name());
    }
    drop(tx); // rx closes when all plugin senders drop

    // ── Inbound message processor ─────────────────────────────────────────────
    spawn_inbound_processor(db.clone(), rx);

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

    // ── HTTP router ───────────────────────────────────────────────────────────
    let state = AppState {
        db,
        plugins: Arc::new(plugins),
        default_channel,
        api_key,
    };

    // API routes require bearer auth.
    let api = Router::new()
        .route("/v1/projects", post(routes::register_project))
        .route("/v1/projects/{ident}/messages", post(routes::send_message))
        .route(
            "/v1/projects/{ident}/messages/unread",
            get(routes::get_unread_messages),
        )
        .route(
            "/v1/projects/{ident}/messages/{id}/confirm",
            post(routes::confirm_message),
        )
        .route("/v1/skills", get(routes::list_skills_handler))
        .route(
            "/v1/skills/{name}",
            put(routes::upload_skill)
                .get(routes::download_skill)
                .delete(routes::delete_skill_handler),
        )
        .layer(middleware::from_fn_with_state(state.clone(), bearer_auth));

    // Dashboard at / is public (local admin page, no auth required).
    let app = Router::new()
        .route("/", get(routes::dashboard))
        .merge(api)
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
