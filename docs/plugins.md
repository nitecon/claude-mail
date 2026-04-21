# Channel Plugin Development Guide

Plugins are how the gateway delivers messages to the user. Each plugin implements one communication medium — Discord, Slack, email, etc. One plugin is assigned per project at registration time; all inbound and outbound messages for that project flow exclusively through that plugin.

The MCP server (in the [agent-tools](https://github.com/nitecon/agent-tools) repo) is completely unaware of plugins. It only speaks HTTP to the gateway. Adding a new plugin requires changes only inside `crates/gateway/`.

---

## How plugins fit into the gateway

```
HTTP handler (routes.rs)
    │ plugin.ensure_room()
    │ plugin.send()
    ▼
Arc<dyn ChannelPlugin>   ← your plugin lives here
    │ pushes PluginEvent::Message { room_id, message }
    ▼
mpsc::Receiver<PluginEvent>
    ▼
Inbound processor (main.rs)
    │ resolves room_id → project_ident via DB
    ▼
messages table (SQLite)
    ▼
GET /v1/projects/:ident/messages/unread
```

The gateway holds a `HashMap<String, Arc<dyn ChannelPlugin>>` keyed by plugin name. Route handlers look up the correct plugin by the `channel_name` stored with each project in the database.

---

## The `ChannelPlugin` trait

Defined in `crates/gateway/src/channel.rs`.

```rust
#[async_trait]
pub trait ChannelPlugin: Send + Sync {
    fn name(&self) -> &str;

    fn register_room(&self, room_id: &str, last_msg_id: Option<&str>);

    async fn start(&self, tx: mpsc::Sender<PluginEvent>) -> Result<()>;

    async fn ensure_room(&self, project_ident: &str) -> Result<String>;

    async fn send(&self, room_id: &str, content: &str) -> Result<String>;

    /// Reply to a specific message in the room. Returns a message ID.
    /// Default implementation falls back to `send()`.
    async fn reply(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        content: &str,
    ) -> Result<String> {
        let _ = reply_to_external_id;
        self.send(room_id, content).await
    }

    /// Post a structured outbound message. The default impl renders the
    /// `OutboundMessage` to markdown and forwards to `send()`. Override to
    /// emit native rich primitives (Discord embeds, Slack blocks, MIME).
    async fn send_structured(&self, room_id: &str, msg: &OutboundMessage) -> Result<String> {
        self.send(room_id, &msg.render_markdown()).await
    }

    /// Structured reply variant. Default impl renders to markdown and
    /// forwards to `reply()`.
    async fn reply_structured(
        &self,
        room_id: &str,
        reply_to_external_id: &str,
        msg: &OutboundMessage,
    ) -> Result<String> {
        self.reply(room_id, reply_to_external_id, &msg.render_markdown()).await
    }

    async fn fetch_since(
        &self,
        room_id: &str,
        after_id: Option<&str>,
    ) -> Result<Vec<InboundMessage>>;
}
```

### Key types

```rust
/// A message received from the user.
pub struct InboundMessage {
    pub id: String,       // opaque, plugin-specific message ID (used as backfill cursor)
    pub content: String,
    pub sender: String,   // human-readable: username, email address, etc.
}

/// A structured outbound message handed to a plugin's `send_structured` /
/// `reply_structured`. Plugins with rich rendering map these fields onto
/// native primitives; others fall through to `render_markdown()` + `send()`.
pub struct OutboundMessage {
    pub agent_id: String,
    pub hostname: String,
    pub subject: String,
    pub body: String,
    pub event_at: i64,    // event time in epoch ms (distinct from gateway-receive time)
}

/// The event your plugin pushes into the shared channel.
pub enum PluginEvent {
    Message {
        channel_name: String,  // must match self.name()
        room_id: String,       // opaque room identifier
        message: InboundMessage,
    },
}
```

### Lifecycle

The gateway calls methods in this order:

```
1. register_room(room_id, last_msg_id)  ← repeated for every existing project at startup
2. start(tx)                            ← called once; plugin begins receiving messages
3. ensure_room(project_ident)           ← called per new project registration
   send_structured(room_id, msg)       ← called per outbound agent message (default impl falls back to send)
   fetch_since(room_id, after_id)      ← called during backfill
```

**`start()` must return promptly.** Spawn background tasks (WebSocket listeners, polling loops) internally and return `Ok(())`. The gateway starts the HTTP server immediately after `start()` returns.

### Method contracts

#### `name() -> &str`

Return a short, lowercase, stable identifier. This string is stored in the database and used in config (`DEFAULT_CHANNEL`, `channel` field of `POST /v1/projects`). Once chosen, never change it — existing projects in the DB reference it.

#### `register_room(room_id, last_msg_id)`

Record that this `room_id` is active. The plugin should:
- Store it in an internal set/map so `message` events from that room are forwarded.
- Store `last_msg_id` as the backfill cursor for `start()`.
- Be idempotent — the same room may be registered more than once.

This is a synchronous method; do not block.

#### `start(tx)`

Connect to the underlying service and begin routing inbound messages into `tx`. For each user message received:

```rust
tx.send(PluginEvent::Message {
    channel_name: self.name().into(),
    room_id: /* the room this message came from */,
    message: InboundMessage {
        id: /* stable, unique message ID */,
        content: msg.content,
        sender: msg.author_name,
    },
}).await?;
```

Also perform backfill here (or in a `ready`/`connected` callback): for each registered room that has a `last_msg_id`, fetch messages after it via `fetch_since` and push them into `tx` before processing live messages.

**Do not hold a `Mutex` guard across any `.await` point.** This will prevent the future from being `Send` and cause a compile error. Always drop guards before awaiting.

#### `ensure_room(project_ident) -> Result<String>`

Create (or find) the room that will represent this project. Return an opaque `room_id` string that you can later use in `send()` and `fetch_since()`.

- **Channel-based** (Discord, Slack): create a channel named `project_ident`, return its ID.
- **Thread-based** (email): derive an address (`project_ident@your-domain`), return it as the room ID. The actual thread is formed lazily on first `send()`.

Before returning, call `self.register_room(&room_id, None)` so the plugin immediately starts accepting inbound messages for the new room.

#### `send(room_id, content) -> Result<String>`

Deliver `content` to the room identified by `room_id`. Return a stable, unique message ID that can be used as a backfill cursor by `fetch_since`. This is the low-level text primitive — most outbound traffic flows through `send_structured`, which renders an `OutboundMessage` and falls back to `send()` for plugins without rich rendering. Do not add any agent prefix yourself; attribution is carried by the structured envelope.

#### `reply(room_id, reply_to_external_id, content) -> Result<String>`

Reply to a specific message in the room, creating a thread or visual reply chain if the platform supports it. `reply_to_external_id` is the plugin-specific external message ID of the message being replied to.

The default implementation ignores `reply_to_external_id` and falls back to `send()`. Override this if your platform supports native threading (e.g. Discord message references, Slack threads). The Discord plugin implements this by setting a `reference_message` on the outgoing message.

#### `send_structured(room_id, msg) -> Result<String>` *(default provided)*

Deliver an `OutboundMessage` (subject, agent/hostname byline, body, event timestamp) to the room. Override this when your platform has rich primitives that map naturally onto the envelope:

- **Discord** renders an embed: `subject` becomes the title, `agent_id · hostname` becomes the embed author, `body` is wrapped in a fenced code block as the description, and `event_at` becomes the embed timestamp.
- **Slack** would build Block Kit sections.
- **Email / MIME** would map `subject` to the `Subject` header and `body` to the message body.

The default implementation calls `OutboundMessage::render_markdown()` (bold subject + italic byline + fenced body) and forwards to `send()`. Triple-backticks in the body are sanitized by `render_markdown` so user content cannot break out of the fence — preserve that behavior in your own renderer.

#### `reply_structured(room_id, reply_to_external_id, msg) -> Result<String>` *(default provided)*

Structured analogue of `reply`. Default impl renders the envelope to markdown and forwards to `reply` (which itself falls back to `send` on plugins without native threading). Override when your platform supports both rich rendering AND threading.

#### `fetch_since(room_id, after_id) -> Result<Vec<InboundMessage>>`

Return all user messages in `room_id` that arrived after `after_id` (exclusive), in ascending chronological order. If `after_id` is `None`, return the most recent messages (up to a reasonable limit, e.g. 100). Filter out bot/automated messages — only return human-authored content.

---

## Writing a new plugin

### 1. Create the file

Add `crates/gateway/src/channels/your_plugin.rs`. The Discord plugin (`channels/discord.rs`) is the reference implementation.

A minimal skeleton:

```rust
use anyhow::Result;
use async_trait::async_trait;
use std::sync::Mutex;
use std::collections::HashMap;
use tokio::sync::mpsc;

use crate::channel::{ChannelPlugin, InboundMessage, PluginEvent};

pub struct YourConfig {
    // API keys, endpoints, etc.
}

pub struct YourPlugin {
    config: YourConfig,
    // room_id → Option<last_msg_id>
    rooms: Mutex<HashMap<String, Option<String>>>,
}

impl YourPlugin {
    pub fn new(config: YourConfig) -> Self {
        Self {
            config,
            rooms: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ChannelPlugin for YourPlugin {
    fn name(&self) -> &str {
        "your-plugin"    // unique, lowercase, stable
    }

    fn register_room(&self, room_id: &str, last_msg_id: Option<&str>) {
        self.rooms
            .lock()
            .unwrap()
            .entry(room_id.to_string())
            .or_insert_with(|| last_msg_id.map(String::from));
    }

    async fn start(&self, tx: mpsc::Sender<PluginEvent>) -> Result<()> {
        // Snapshot rooms before any await (never hold Mutex across .await).
        let rooms: Vec<(String, Option<String>)> = self
            .rooms
            .lock()
            .unwrap()
            .clone()
            .into_iter()
            .collect();

        // Backfill missed messages for each known room.
        for (room_id, last_msg_id) in &rooms {
            for msg in self.fetch_since(room_id, last_msg_id.as_deref()).await? {
                tx.send(PluginEvent::Message {
                    channel_name: self.name().into(),
                    room_id: room_id.clone(),
                    message: msg,
                })
                .await
                .ok();
            }
        }

        // Spawn a background task to receive live messages.
        let tx_bg = tx.clone();
        let plugin_name = self.name().to_string();
        // let state = Arc::new(/* whatever you need */);
        tokio::spawn(async move {
            // connect, poll, or subscribe — push PluginEvent::Message into tx_bg
            let _ = (tx_bg, plugin_name); // remove this line in your implementation
        });

        Ok(())
    }

    async fn ensure_room(&self, project_ident: &str) -> Result<String> {
        // Create or find the room. Return a stable room_id.
        let room_id = project_ident.to_string(); // replace with real logic
        self.register_room(&room_id, None);
        Ok(room_id)
    }

    async fn send(&self, room_id: &str, content: &str) -> Result<String> {
        // Deliver content to room_id. Return a stable message ID.
        let message_id = "placeholder-id".to_string(); // replace with real logic
        let _ = (room_id, content);
        Ok(message_id)
    }

    async fn fetch_since(
        &self,
        room_id: &str,
        after_id: Option<&str>,
    ) -> Result<Vec<InboundMessage>> {
        // Return user messages after after_id in ascending order.
        let _ = (room_id, after_id);
        Ok(vec![])
    }
}
```

### 2. Register in `channels/mod.rs`

```rust
#[cfg(feature = "discord")]
pub mod discord;

#[cfg(feature = "your-plugin")]
pub mod your_plugin;
```

### 3. Add a Cargo feature in `crates/gateway/Cargo.toml`

```toml
[features]
default = ["discord"]
discord     = ["dep:serenity"]
your-plugin = ["dep:some-crate"]

[dependencies]
some-crate = { version = "x.y", optional = true }
```

Keeping the dependency optional means it is only compiled when the feature is enabled:

```bash
cargo build --features your-plugin          # alongside discord (default)
cargo build --no-default-features --features your-plugin   # standalone
```

### 4. Wire it into `main.rs`

Inside the plugin registry block in `crates/gateway/src/main.rs`:

```rust
#[cfg(feature = "your-plugin")]
{
    let api_key = require_env("YOUR_PLUGIN_API_KEY");
    // read any other config vars your plugin needs
    let plugin = Arc::new(channels::your_plugin::YourPlugin::new(
        channels::your_plugin::YourConfig { api_key },
    ));
    plugins.insert("your-plugin".into(), plugin);
    info!("Registered channel plugin: your-plugin");
}
```

### 5. Document config variables

Add the new variables to `crates/gateway/.env.example` with comments:

```env
# --- your-plugin ---
YOUR_PLUGIN_API_KEY=
# Add any other variables your plugin needs
```

---

## Room ID conventions

The `room_id` is opaque to the gateway — it is stored in the database and passed back to your plugin verbatim. Choose a format that:

- Is **stable**: the same room should always produce the same ID.
- Is **unique within your plugin**: two different projects must not share a room ID.
- Is **a valid string**: no length limit is enforced by the gateway, but keep it reasonable.

| Plugin type | Typical room ID |
|-------------|----------------|
| Discord | Discord channel snowflake: `"1234567890123456789"` |
| Slack | Slack channel ID: `"C0123ABCDEF"` |
| Email | Email address or thread Message-ID: `"bruce@mail.example.com"` |
| AgentMail | Mailbox or thread identifier: `"thread:bruce:2026"` |

---

## Backfill and the `last_msg_id` cursor

The gateway stores one `last_msg_id` per project in the `projects` table. The inbound processor updates it every time a user message is inserted:

```
User sends message → plugin pushes PluginEvent → inbound processor inserts row
                                                 → updates projects.last_msg_id
```

On gateway restart:

1. `main.rs` loads all projects from the DB and calls `register_room(room_id, last_msg_id)` for each.
2. Your plugin stores these cursors.
3. `start()` is called — your plugin backfills using `fetch_since(room_id, last_msg_id)` and pushes any missed messages.

Your `fetch_since` must treat `after_id` as **exclusive** (do not return the message with that ID, only messages after it).

---

## Thread safety rules

The gateway uses `tokio` with a multi-threaded runtime. Your plugin will be accessed concurrently from:

- The HTTP handler tasks (calling `ensure_room` and `send`).
- Your own background task (pushing events into `tx`).

All shared mutable state must be behind `Arc<Mutex<_>>` or `Arc<RwLock<_>>`. **Never hold a lock guard across an `.await` point** — extract the data you need, drop the guard, then await:

```rust
// ✓ correct
let data = {
    self.rooms.lock().unwrap().get(room_id).cloned()
}; // guard dropped here
some_async_call(data).await?;

// ✗ wrong — compile error: future is not Send
let guard = self.rooms.lock().unwrap();
some_async_call(guard.get(room_id)).await?;
```

---

## Testing your plugin

The quickest path to a working integration:

1. Start the gateway with your plugin enabled:
   ```bash
   cargo run -p gateway --no-default-features --features your-plugin
   ```

2. Register a test project via curl:
   ```bash
   curl -s -X POST http://localhost:3000/v1/projects \
     -H "Authorization: Bearer $GATEWAY_API_KEY" \
     -H "Content-Type: application/json" \
     -d '{"ident": "test-project", "channel": "your-plugin"}'
   ```
   Verify the room was created in the underlying service.

3. Send an agent message:
   ```bash
   curl -s -X POST http://localhost:3000/v1/projects/test-project/messages \
     -H "Authorization: Bearer $GATEWAY_API_KEY" \
     -H "Content-Type: application/json" \
     -d '{"subject": "smoke test", "body": "hello from agent"}'
   ```
   (The legacy field name `content` is still accepted as an alias for `body`.)
   Verify it appears in the service (inbox, channel, etc.).

4. Reply as a user in the service, then poll:
   ```bash
   curl -s http://localhost:3000/v1/projects/test-project/messages/unread \
     -H "Authorization: Bearer $GATEWAY_API_KEY"
   ```
   Verify your reply appears in the response.

5. Poll again — response should be `"no messages"` (cursor advanced).

6. Restart the gateway and poll again — backfill should recover any messages sent while it was down.
