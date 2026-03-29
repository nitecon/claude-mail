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
use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::channel::{ChannelPlugin, InboundMessage, PluginEvent};

// ── Config ────────────────────────────────────────────────────────────────────

pub struct DiscordConfig {
    pub token: String,
    pub guild_id: u64,
    pub category_id: Option<u64>,
}

// ── Plugin ────────────────────────────────────────────────────────────────────

/// Rooms known to this plugin: Discord channel ID (u64) → last seen message ID.
type RoomMap = Mutex<HashMap<u64, Option<String>>>;

pub struct DiscordPlugin {
    config: DiscordConfig,
    /// channel_id → Option<last_msg_id>  (populated via register_room, persisted in DB)
    rooms: RoomMap,
    /// Set after start() is called; used for REST operations.
    http: OnceLock<std::sync::Arc<serenity::http::Http>>,
}

impl DiscordPlugin {
    pub fn new(config: DiscordConfig) -> Self {
        Self {
            config,
            rooms: Mutex::new(HashMap::new()),
            http: OnceLock::new(),
        }
    }

    fn guild(&self) -> GuildId {
        GuildId::new(self.config.guild_id)
    }

    fn category(&self) -> Option<ChannelId> {
        self.config.category_id.map(ChannelId::new)
    }

    fn http(&self) -> &std::sync::Arc<serenity::http::Http> {
        self.http.get().expect("DiscordPlugin::start() must be called before HTTP operations")
    }
}

// ── ChannelPlugin impl ────────────────────────────────────────────────────────

#[async_trait]
impl ChannelPlugin for DiscordPlugin {
    fn name(&self) -> &str {
        "discord"
    }

    fn register_room(&self, room_id: &str, last_msg_id: Option<&str>) {
        if let Ok(id) = room_id.parse::<u64>() {
            self.rooms
                .lock()
                .unwrap()
                .entry(id)
                .or_insert_with(|| last_msg_id.map(String::from));
        }
    }

    async fn start(&self, tx: mpsc::Sender<PluginEvent>) -> Result<()> {
        // Snapshot rooms for the handler (clone is cheap — small map).
        let rooms_snapshot = self.rooms.lock().unwrap().clone();

        let handler = DiscordHandler {
            rooms: std::sync::Arc::new(Mutex::new(rooms_snapshot)),
            tx,
        };

        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILDS;

        let mut client = Client::builder(&self.config.token, intents)
            .event_handler(handler)
            .await
            .context("build Discord client")?;

        // Store HTTP handle for REST calls from other trait methods.
        let _ = self.http.set(client.http.clone());

        tokio::spawn(async move {
            if let Err(e) = client.start().await {
                error!("Discord gateway error: {e}");
            }
        });

        Ok(())
    }

    async fn ensure_room(&self, project_ident: &str) -> Result<String> {
        let http = self.http();
        let guild = self.guild();

        // Check if channel already exists.
        let channels: HashMap<ChannelId, GuildChannel> =
            guild.channels(http).await.context("fetch guild channels")?;

        if let Some(ch) = channels.values().find(|c| c.name == project_ident) {
            let room_id = ch.id.get().to_string();
            self.register_room(&room_id, None);
            return Ok(room_id);
        }

        // Create it.
        let mut builder = CreateChannel::new(project_ident).kind(ChannelType::Text);
        if let Some(cat) = self.category() {
            builder = builder.category(cat);
        }

        let channel = guild
            .create_channel(http, builder)
            .await
            .context("create Discord channel")?;

        let room_id = channel.id.get().to_string();
        self.register_room(&room_id, None);
        info!("Created Discord channel #{project_ident} (id={room_id})");
        Ok(room_id)
    }

    async fn send(&self, room_id: &str, content: &str) -> Result<String> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let ch = ChannelId::new(id);
        let msg = ch
            .send_message(self.http(), CreateMessage::new().content(content))
            .await
            .context("send Discord message")?;
        Ok(msg.id.to_string())
    }

    async fn fetch_since(
        &self,
        room_id: &str,
        after_id: Option<&str>,
    ) -> Result<Vec<InboundMessage>> {
        let id: u64 = room_id.parse().context("parse Discord channel id")?;
        let ch = ChannelId::new(id);

        let builder = match after_id {
            Some(after) => {
                let snowflake: u64 = after.parse().context("parse after_id snowflake")?;
                GetMessages::new().after(MessageId::new(snowflake)).limit(100)
            }
            None => GetMessages::new().limit(100),
        };

        let msgs = ch
            .messages(self.http(), builder)
            .await
            .context("fetch Discord messages")?;

        Ok(msgs
            .into_iter()
            .filter(|m| !m.author.bot)
            .map(|m| InboundMessage {
                id: m.id.to_string(),
                content: m.content,
                sender: m.author.name,
            })
            .collect())
    }
}

// ── Event handler ─────────────────────────────────────────────────────────────

struct DiscordHandler {
    /// Local copy of the room map for this gateway session.
    rooms: std::sync::Arc<Mutex<HashMap<u64, Option<String>>>>,
    tx: mpsc::Sender<PluginEvent>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: SerenityCtx, ready: Ready) {
        info!("Discord bot connected as {}", ready.user.name);

        // Clone room map before any await so we never hold the guard across one.
        let rooms: Vec<(u64, Option<String>)> =
            self.rooms.lock().unwrap().clone().into_iter().collect();

        for (channel_id, last_msg_id) in rooms {
            let after = match last_msg_id.as_deref().and_then(|s| s.parse::<u64>().ok()) {
                Some(id) => id,
                None => continue,
            };

            let ch = ChannelId::new(channel_id);
            match ch
                .messages(
                    &ctx.http,
                    GetMessages::new().after(MessageId::new(after)).limit(100),
                )
                .await
            {
                Ok(msgs) => {
                    for msg in msgs.into_iter().filter(|m| !m.author.bot) {
                        let event = PluginEvent::Message {
                            channel_name: "discord".into(),
                            room_id: channel_id.to_string(),
                            message: InboundMessage {
                                id: msg.id.to_string(),
                                content: msg.content,
                                sender: msg.author.name,
                            },
                        };
                        if self.tx.send(event).await.is_err() {
                            return; // gateway shut down
                        }
                    }
                }
                Err(e) => warn!("backfill error for channel {channel_id}: {e}"),
            }
        }
    }

    async fn message(&self, _ctx: SerenityCtx, msg: Message) {
        if msg.author.bot {
            return;
        }

        let channel_id = msg.channel_id.get();

        // Drop the guard before any await.
        let is_known = self.rooms.lock().unwrap().contains_key(&channel_id);
        if !is_known {
            return;
        }

        let event = PluginEvent::Message {
            channel_name: "discord".into(),
            room_id: channel_id.to_string(),
            message: InboundMessage {
                id: msg.id.to_string(),
                content: msg.content,
                sender: msg.author.name,
            },
        };

        if let Err(e) = self.tx.send(event).await {
            error!("failed to forward Discord message to inbound processor: {e}");
        }
    }
}
