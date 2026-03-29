use anyhow::{Context, Result};
use async_trait::async_trait;
use serenity::{
    all::{
        ChannelId, ChannelType, Context as SerenityCtx, CreateChannel, CreateMessage,
        EventHandler, GatewayIntents, GetMessages, GuildChannel, GuildId, Message, MessageId,
        Ready,
    },
    Client,
};
use std::sync::{Arc, Mutex};
use tracing::{error, info, warn};

use crate::db::{self, Db, now_ms};

/// Shared state the event handler needs at runtime.
pub struct HandlerState {
    pub db: Db,
    pub guild_id: GuildId,
    /// Channel IDs belonging to this gateway (project channels only).
    pub project_channel_ids: Arc<Mutex<std::collections::HashSet<u64>>>,
}

struct DiscordHandler {
    state: Arc<HandlerState>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: SerenityCtx, ready: Ready) {
        info!("Discord bot connected as {}", ready.user.name);

        // Load all projects from DB (synchronous — lock dropped before any await).
        let projects = {
            let conn = self.state.db.lock().unwrap();
            db::all_projects(&conn).unwrap_or_default()
        };

        // Register all known project channel IDs (lock dropped before any await).
        {
            let mut channel_ids = self.state.project_channel_ids.lock().unwrap();
            for project in &projects {
                if let Ok(id) = project.discord_channel_id.parse::<u64>() {
                    channel_ids.insert(id);
                }
            }
        }

        // Backfill any messages received while the bot was offline (async work).
        for project in &projects {
            let channel_id = match project.discord_channel_id.parse::<u64>() {
                Ok(id) => ChannelId::new(id),
                Err(_) => continue,
            };

            let last_id = match &project.last_discord_msg_id {
                Some(id) => id.clone(),
                None => continue,
            };

            let after = match last_id.parse::<u64>() {
                Ok(id) => MessageId::new(id),
                Err(_) => continue,
            };

            match channel_id
                .messages(&ctx.http, GetMessages::new().after(after).limit(100))
                .await
            {
                Ok(msgs) => {
                    let conn = self.state.db.lock().unwrap();
                    for msg in msgs.iter().filter(|m| !m.author.bot) {
                        let m = db::Message {
                            id: 0,
                            project_ident: project.ident.clone(),
                            source: "user".into(),
                            discord_message_id: Some(msg.id.to_string()),
                            content: msg.content.clone(),
                            sent_at: now_ms(),
                        };
                        if let Err(e) = db::insert_message(&conn, &m) {
                            warn!("backfill insert error for {}: {}", project.ident, e);
                        }
                    }
                    if let Some(last_msg) = msgs.last() {
                        let _ = db::update_last_discord_msg_id(
                            &conn,
                            &project.ident,
                            &last_msg.id.to_string(),
                        );
                    }
                }
                Err(e) => warn!("backfill fetch error for {}: {}", project.ident, e),
            }
        }
    }

    async fn message(&self, _ctx: SerenityCtx, msg: Message) {
        // Ignore bot messages (including our own outbound ones).
        if msg.author.bot {
            return;
        }

        let channel_id_u64 = msg.channel_id.get();

        let is_project_channel = {
            self.state
                .project_channel_ids
                .lock()
                .unwrap()
                .contains(&channel_id_u64)
        };
        if !is_project_channel {
            return;
        }

        // Find which project owns this channel (synchronous, no await).
        let project_ident = {
            let conn = self.state.db.lock().unwrap();
            db::all_projects(&conn)
                .unwrap_or_default()
                .into_iter()
                .find(|p| p.discord_channel_id == channel_id_u64.to_string())
                .map(|p| p.ident)
        };

        if let Some(ident) = project_ident {
            let discord_id = msg.id.to_string();
            let m = db::Message {
                id: 0,
                project_ident: ident.clone(),
                source: "user".into(),
                discord_message_id: Some(discord_id.clone()),
                content: msg.content.clone(),
                sent_at: now_ms(),
            };
            let conn = self.state.db.lock().unwrap();
            if let Err(e) = db::insert_message(&conn, &m) {
                error!("failed to store user message for {}: {}", ident, e);
                return;
            }
            let _ = db::update_last_discord_msg_id(&conn, &ident, &discord_id);
        }
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Handle to the Discord HTTP client for use from route handlers.
pub struct DiscordHttp {
    pub http: Arc<serenity::http::Http>,
    pub guild_id: GuildId,
    pub category_id: Option<ChannelId>,
}

/// Start the serenity gateway client and return an HTTP handle for REST calls.
/// Spawns the gateway event loop as a background task.
///
/// `project_channel_ids` must be the same Arc used in `AppState` so that
/// channels registered via the HTTP API are visible to the event handler.
pub async fn start(
    token: &str,
    guild_id: u64,
    category_id: Option<u64>,
    db: Db,
    project_channel_ids: Arc<Mutex<std::collections::HashSet<u64>>>,
) -> Result<Arc<DiscordHttp>> {
    let guild = GuildId::new(guild_id);
    let category = category_id.map(ChannelId::new);

    let handler_state = Arc::new(HandlerState {
        db,
        guild_id: guild,
        project_channel_ids: project_channel_ids.clone(),
    });

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILDS;

    let handler = DiscordHandler {
        state: handler_state,
    };

    let mut client = Client::builder(token, intents)
        .event_handler(handler)
        .await
        .context("build Discord client")?;

    let http = client.http.clone();

    tokio::spawn(async move {
        if let Err(e) = client.start().await {
            error!("Discord client error: {e}");
        }
    });

    Ok(Arc::new(DiscordHttp {
        http,
        guild_id: guild,
        category_id: category,
    }))
}

/// Ensure a text channel named `ident` exists in the guild.
/// Returns the channel ID as a string.
pub async fn ensure_channel(
    discord: &DiscordHttp,
    ident: &str,
    project_channel_ids: &Arc<Mutex<std::collections::HashSet<u64>>>,
) -> Result<String> {
    // Check if a channel with that name already exists.
    let channels: std::collections::HashMap<ChannelId, GuildChannel> = discord
        .guild_id
        .channels(&discord.http)
        .await
        .context("fetch guild channels")?;

    if let Some(ch) = channels.values().find(|c| c.name == ident) {
        let id = ch.id.get();
        project_channel_ids.lock().unwrap().insert(id);
        return Ok(id.to_string());
    }

    // Create it.
    let mut builder = CreateChannel::new(ident).kind(ChannelType::Text);
    if let Some(cat_id) = discord.category_id {
        builder = builder.category(cat_id);
    }

    let channel = discord
        .guild_id
        .create_channel(&discord.http, builder)
        .await
        .context("create Discord channel")?;

    let id = channel.id.get();
    project_channel_ids.lock().unwrap().insert(id);
    info!("Created Discord channel #{ident} (id={id})");
    Ok(id.to_string())
}

/// Post a message to a channel and return the Discord message ID.
pub async fn post_message(
    discord: &DiscordHttp,
    channel_id: &str,
    content: &str,
) -> Result<String> {
    let id: u64 = channel_id.parse().context("parse channel id")?;
    let ch = ChannelId::new(id);
    let msg = ch
        .send_message(&discord.http, CreateMessage::new().content(content))
        .await
        .context("send Discord message")?;
    Ok(msg.id.to_string())
}
