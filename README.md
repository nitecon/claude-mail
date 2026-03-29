# claude-mail

Async communication between AI agents and humans via Discord.

AI agents (Claude, Gemini, Codex, etc.) often run unattended for extended periods. claude-mail gives them a way to send you messages and check for your replies — without you needing to watch a terminal. Each project gets its own Discord channel, messages are persisted locally, and multiple agent sessions across different machines share the same channel.

## How it works

```
AI Agent (Claude Code, etc.)
    │  MCP stdio protocol
    ▼
claude-mail binary          ← installed per machine
    │  HTTP + Bearer token
    ▼
Gateway service             ← one persistent service on your network
    │  Discord WebSocket / REST
    ▼
Discord server              ← one channel per project (e.g. #bruce)
    │
    ▼
You (the user)
```

1. An agent calls `set_identity` with the project's git remote URL or directory name.
2. The gateway ensures a Discord channel exists for that project (creates it if not).
3. The agent calls `send_message` — the message appears in Discord prefixed with `[AGENT]`.
4. You reply in Discord.
5. The agent calls `get_messages` — it receives everything since its last check.

Multiple machines running agents on the same project share the same channel and message history.

## Project layout

```
claude-mail/
├── crates/
│   ├── gateway/        # Persistent HTTP service + Discord bot
│   └── mcp-server/     # stdio MCP server (installed per machine)
```

## Prerequisites

- Rust (stable, 1.75+) — [rustup.rs](https://rustup.rs)
- A Discord account and server you control
- The gateway reachable from all machines running the MCP server (LAN, VPN, or public host)

---

## Getting started

### 1. Clone and build

```bash
git clone https://github.com/yourorg/claude-mail
cd claude-mail
cargo build --release
```

Binaries will be at:
- `target/release/gateway`
- `target/release/claude-mail`

---

### 2. Create a Discord bot

1. Go to [discord.com/developers/applications](https://discord.com/developers/applications) and click **New Application**.
2. Under **Bot**, click **Add Bot**.
3. Copy the **Bot Token** — you will need it for `DISCORD_BOT_TOKEN`.
4. Under **Privileged Gateway Intents**, enable:
   - **Server Members Intent** (optional but recommended)
   - **Message Content Intent** — **required** for the bot to read message text
5. Under **OAuth2 → URL Generator**, select scopes: `bot`. Select bot permissions:
   - `Manage Channels`
   - `Read Messages / View Channels`
   - `Send Messages`
   - `Read Message History`
6. Open the generated URL and add the bot to your Discord server.

---

### 3. Get your server and category IDs

Enable **Developer Mode** in Discord (Settings → Advanced → Developer Mode), then:

- **Guild ID**: Right-click your server name → **Copy Server ID**.
- **Category ID** (optional): Right-click a category → **Copy Category ID**. If set, all project channels are created inside this category.

---

### 4. Configure the gateway

```bash
cd crates/gateway
cp .env.example .env
```

Edit `.env`:

```env
DISCORD_BOT_TOKEN=your-bot-token-here
DISCORD_GUILD_ID=123456789012345678
DISCORD_CATEGORY_ID=                  # optional — leave blank for top-level channels
GATEWAY_API_KEY=choose-a-long-random-secret
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=3000
DATABASE_PATH=./data/claude-mail.db
MESSAGE_RETENTION_DAYS=30
RUST_LOG=info
```

> `GATEWAY_API_KEY` is the shared secret between the gateway and all MCP server instances. Use a long random string (e.g. `openssl rand -hex 32`).

---

### 5. Start the gateway

```bash
# From the repo root
cargo run --release -p gateway

# Or run the binary directly
./target/release/gateway
```

You should see:

```
INFO gateway: SQLite database opened at ./data/claude-mail.db
INFO gateway: Discord bot started (guild=123456789012345678)
INFO gateway: Gateway listening on http://0.0.0.0:3000
INFO gateway::discord: Discord bot connected as YourBotName#1234
```

For production use, run this under a process manager (systemd, PM2, etc.) so it restarts on reboot.

---

### 6. Configure the MCP server

On each machine where you want agents to use claude-mail:

```bash
cd crates/mcp-server
cp .env.example .env
```

Edit `.env`:

```env
GATEWAY_URL=http://192.168.1.100:3000   # IP/hostname of the machine running the gateway
GATEWAY_API_KEY=choose-a-long-random-secret   # must match the gateway's key
DEFAULT_PROJECT_IDENT=                  # optional — auto-sets identity on startup
GATEWAY_TIMEOUT_MS=5000
RUST_LOG=info
```

---

### 7. Add to Claude Code

In your project's `.claude/settings.json` (or globally in `~/.claude/settings.json`):

```json
{
  "mcpServers": {
    "claude-mail": {
      "type": "stdio",
      "command": "/absolute/path/to/claude-mail",
      "env": {
        "GATEWAY_URL": "http://192.168.1.100:3000",
        "GATEWAY_API_KEY": "your-secret-here"
      }
    }
  }
}
```

> If you set `DEFAULT_PROJECT_IDENT` in the env block, `set_identity` is called automatically on startup.

For other MCP-compatible agents (Gemini, Codex, etc.), consult their documentation for adding a stdio MCP server — the binary and environment variables are the same.

---

### 8. Instruct your agent

Add something like this to your project's `CLAUDE.md`:

```markdown
## Communication

Use the `claude-mail` MCP server to stay in contact with the user.

1. At the start of each session, call `set_identity` with the project identity.
   Use the git remote URL if available (e.g. `github.com/nitecon/bruce.git`),
   otherwise use the current directory name.
2. When you need input, have a question, or want to report significant progress,
   call `send_message`.
3. Before starting a long task, call `get_messages` to check for new instructions.
```

---

## MCP tools reference

| Tool | Parameters | Description |
|------|-----------|-------------|
| `set_identity` | `project_ident: string` | Set the project identity for this session. Must be called first. Accepts a git remote URL or directory name — sanitized into a Discord channel name automatically. |
| `send_message` | `content: string` | Send a message to the user via the project's Discord channel. Appears as `[AGENT] your message`. |
| `get_messages` | _(none)_ | Fetch unread messages since the last call. Returns `[AGENT]` and `[USER]` prefixed lines, or `"no messages"`. |

**Identity examples:**

| Input | Discord channel |
|-------|----------------|
| `github.com/nitecon/bruce.git` | `#bruce` |
| `github.com/org/my-api-service` | `#my-api-service` |
| `/home/user/projects/bruce` | `#bruce` |
| `C:\Users\nitec\Documents\Projects\bruce` | `#bruce` |

---

## Gateway API reference

The gateway exposes a simple REST API. All endpoints require `Authorization: Bearer <GATEWAY_API_KEY>`.

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/projects` | Register a project. Creates the Discord channel if it doesn't exist. Idempotent. |
| `POST` | `/v1/projects/:ident/messages` | Send an agent message. Body: `{"content": "..."}` |
| `GET` | `/v1/projects/:ident/messages/unread` | Get unread messages and advance the read cursor. |

---

## Multi-machine setup

If you have the same project running on multiple machines (e.g. Windows dev box and a Linux build server), point both MCP server instances at the same gateway with the same project identity.

Both instances share:
- The same Discord channel
- The same message history
- The same read cursor (last `get_messages` call from either machine advances it for both)

This also enables cross-machine coordination: an agent on Linux can send a message like "build succeeds on Linux but failing on Windows — user input needed", and you can reply in Discord to direct both agents.

---

## Troubleshooting

**Bot doesn't receive user messages**
Ensure **Message Content Intent** is enabled in the Discord Developer Portal under your bot's settings. Without it, `message.content` is always empty.

**`get_messages` returns old messages repeatedly**
The read cursor is shared across all agent instances for a project. If a second agent instance calls `get_messages`, it advances the cursor, so the first instance won't see those messages. This is by design for v1.

**Gateway can't create channels**
The bot needs **Manage Channels** permission in your Discord server. Re-invite it using the OAuth2 URL Generator with that permission checked.

**MCP server can't reach the gateway**
Check that `GATEWAY_URL` is correct and the gateway's port is reachable from the machine running the MCP server. On LAN, ensure your firewall allows the connection.
