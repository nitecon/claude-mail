use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::mpsc;

// ── Inbound types ─────────────────────────────────────────────────────────────

/// A message received from a human via any channel plugin.
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// Opaque, plugin-specific message identifier (used as backfill cursor).
    pub id: String,
    pub content: String,
    /// Human-readable sender (username, email address, etc.).
    pub sender: String,
}

/// Events pushed by a plugin into the gateway's inbound processor.
#[derive(Debug)]
pub enum PluginEvent {
    Message {
        /// Name of the plugin that produced this event ("discord", "slack", …).
        channel_name: String,
        /// Opaque room identifier (Discord channel ID, Slack channel ID, email thread ID, …).
        room_id: String,
        message: InboundMessage,
    },
}

// ── Plugin trait ──────────────────────────────────────────────────────────────

/// A communication back-channel between AI agents and the user.
///
/// Each implementation handles one medium (Discord, Slack, email, …).
/// One plugin is assigned per project at registration time; the gateway
/// routes all outbound and inbound messages for that project through
/// the assigned plugin.
///
/// Lifecycle:
///   1. `register_room()` — called at startup for every existing project,
///      and immediately after `ensure_room()` for new ones.
///   2. `start()` — called once after all rooms are registered; the plugin
///      begins receiving inbound messages and pushes them into `tx`.
///   3. `ensure_room()` / `send()` / `fetch_since()` — called by HTTP handlers.
#[async_trait]
pub trait ChannelPlugin: Send + Sync {
    /// Short lowercase identifier used in config and the database ("discord", "slack", …).
    fn name(&self) -> &str;

    /// Register a known room so the plugin can accept inbound messages from it.
    ///
    /// `last_msg_id` is the opaque ID of the last message seen in this room;
    /// the plugin will backfill anything after it during `start()`.
    /// Pass `None` for newly-created rooms.
    fn register_room(&self, room_id: &str, last_msg_id: Option<&str>);

    /// Start receiving inbound messages and push them into `tx`.
    ///
    /// Must be called after all `register_room()` calls so that backfill
    /// has the correct cursors. Spawns any background tasks internally and
    /// returns promptly.
    async fn start(&self, tx: mpsc::Sender<PluginEvent>) -> Result<()>;

    /// Ensure a "room" for this project exists in the underlying service.
    ///
    /// - Discord / Slack: create or find a text channel named `project_ident`.
    /// - Email / AgentMail: derive an address or thread identifier.
    ///
    /// Returns an opaque `room_id` that is stored in the database and passed
    /// back to `send()` and `fetch_since()`.
    ///
    /// Implementations must call `register_room(room_id, None)` before returning.
    async fn ensure_room(&self, project_ident: &str) -> Result<String>;

    /// Post a message to the room. Returns an opaque message ID.
    async fn send(&self, room_id: &str, content: &str) -> Result<String>;

    /// Reply to a specific message in the room. Returns an opaque message ID.
    /// `reply_to_external_id` is the plugin-specific ID of the message being replied to.
    /// Default: falls back to `send()` for plugins without native threading.
    async fn reply(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        content: &str,
    ) -> Result<String> {
        let _ = reply_to_external_id;
        self.send(room_id, content).await
    }

    /// Fetch messages received in `room_id` after `after_id`.
    ///
    /// Used during backfill (reconnect / startup). Returns messages in
    /// ascending chronological order.
    async fn fetch_since(
        &self,
        room_id: &str,
        after_id: Option<&str>,
    ) -> Result<Vec<InboundMessage>>;
}
