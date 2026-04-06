# agent-comms

Async communication between AI agents and humans via Discord, with cross-machine skill sharing.

AI agents (Claude, Gemini, Codex, etc.) often run unattended for extended periods. agent-comms gives them a way to send you messages and check for your replies — without you needing to watch a terminal. Each project gets its own Discord channel, messages are persisted locally, and multiple agent sessions across different machines share the same channel.

## How it works

```
AI Agent (Claude Code, etc.)
    │  MCP stdio protocol
    ▼
agent-comms binary          ← installed per machine
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
agent-comms/
├── crates/
│   ├── gateway/        # Persistent HTTP service + Discord bot
│   ├── mcp-server/     # stdio MCP server (installed per machine)
│   └── sync-cli/       # CLI for pushing/pulling shared skills
```

---

## Installation

### Install scripts (recommended)

The install scripts automatically detect your platform, download the latest release, and place the binaries in `/opt/agentic/bin/`.

**Linux / macOS**

```bash
curl -fsSL https://raw.githubusercontent.com/nitecon/agent-comms/main/install.sh | sudo bash
```

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/nitecon/agent-comms/main/install.ps1 | iex
```

This installs three binaries:
- `agent-comms` — MCP server (per-machine)
- `gateway` — Gateway service
- `agent-sync` — Skills management CLI

### Build from source

```bash
git clone https://github.com/nitecon/agent-comms
cd agent-comms
cargo build --release
```

Binaries:
- `target/release/gateway`
- `target/release/agent-comms`
- `target/release/agent-sync`

---

## Prerequisites

- A Discord account and server you control
- The gateway reachable from all machines running the MCP server (LAN, VPN, or public host)

---

## Getting started

### 1. Create a Discord bot

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

### 2. Get your server and category IDs

Enable **Developer Mode** in Discord (Settings → Advanced → Developer Mode), then:

- **Guild ID**: Right-click your server name → **Copy Server ID**.
- **Category ID** (optional): Right-click a category → **Copy Category ID**. If set, all project channels are created inside this category.

---

### 3. Configure the gateway

Copy the example env file and fill in your values:

```bash
cp crates/gateway/.env.example crates/gateway/.env
```

```env
DISCORD_BOT_TOKEN=your-bot-token-here
DISCORD_GUILD_ID=123456789012345678
DISCORD_CATEGORY_ID=                  # optional — leave blank for top-level channels
GATEWAY_API_KEY=choose-a-long-random-secret
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913
DATABASE_PATH=./data/agent-comms.db
MESSAGE_RETENTION_DAYS=30
RUST_LOG=info
```

> `GATEWAY_API_KEY` is the shared secret between the gateway and all clients. Use a long random string (e.g. `openssl rand -hex 32`).

---

### 4. Start the gateway

```bash
./gateway
# or from source: cargo run --release -p gateway
```

You should see:

```
INFO gateway: SQLite database opened at ./data/agent-comms.db
INFO gateway: Gateway listening on http://0.0.0.0:7913
INFO gateway::discord: Discord bot connected as YourBotName#1234
```

The dashboard is available at `http://localhost:7913/` — shows projects, message counts, and skills.

#### Systemd (Linux)

For a full production setup on Linux (dedicated service user, hardened unit file, journald logging) see **[docs/gateway-setup.md](docs/gateway-setup.md)**.

---

### 5. Configure the MCP server (interactive)

On each machine where agents will run:

```bash
agent-comms init
```

This prompts for your gateway URL and API key, then writes `~/.claude/agent-comms.conf`.

To add agent-comms to Claude Code:

```bash
claude mcp add agent-comms -- /opt/agentic/bin/agent-comms
```

> **Note:** `agent-comms` is a **stdio MCP server** — Claude Code spawns it as a local subprocess and communicates over stdin/stdout. There is no HTTP SSE endpoint. The binary makes outbound HTTP calls to the gateway on your behalf.

If you skipped `agent-comms init` or want to override the URL / key inline:

```bash
claude mcp add agent-comms -- /opt/agentic/bin/agent-comms \
  --url=https://your-gateway.example.com \
  --api-key=YOUR_GATEWAY_API_KEY
```

Behind a reverse proxy (nginx/Caddy/etc.) just use the HTTPS URL with no port — the binary will connect to it like any other HTTPS endpoint.

---

### 6. Instruct your agent

Add something like this to your project's `CLAUDE.md`:

```markdown
## Communication

Use the `agent-comms` MCP server to stay in contact with the user.

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
| `set_identity` | `project_ident: string`, `channel?: string` | Set the project identity for this session. Must be called first. Accepts a git remote URL or directory name. Optional `channel` overrides the default plugin (e.g. `"discord"`). |
| `send_message` | `content: string` | Send a message to the user via the project's channel. Appears as `[AGENT] your message`. |
| `get_messages` | _(none)_ | Fetch unread messages since the last call. Returns `[AGENT]` and `[USER]` prefixed lines, or `"no messages"`. |

**Identity examples:**

| Input | Channel name |
|-------|-------------|
| `github.com/nitecon/bruce.git` | `bruce` |
| `github.com/org/my-api-service` | `my-api-service` |
| `/home/user/projects/bruce` | `bruce` |
| `C:\Users\nitec\Documents\Projects\bruce` | `bruce` |

---

## Sync CLI reference

`agent-sync` manages shared Claude Code skills on the gateway. A skill is any directory containing a `SKILL.md` file.

```bash
# Upload a skill directory to the gateway
agent-sync push ~/.claude/skills/my-skill

# Download a skill from the gateway
agent-sync pull my-skill --to ~/.claude/skills

# List all skills on the gateway
agent-sync list

# Delete a skill from the gateway
agent-sync delete my-skill

# Bidirectional sync: push new/changed local, pull new remote
agent-sync sync --dir ~/.claude/skills
```

Configuration uses the same `~/.claude/agent-comms.conf` written by `agent-comms init`, or CLI flags:

```bash
agent-sync --url http://your-gateway:7913 --api-key <key> list
```

---

## Gateway API reference

All endpoints require `Authorization: Bearer <GATEWAY_API_KEY>`.

### Messaging

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/projects` | Register a project. Creates the channel if needed. Idempotent. Body: `{"ident": "...", "channel": "discord"}` |
| `POST` | `/v1/projects/:ident/messages` | Send an agent message. Body: `{"content": "..."}` |
| `GET` | `/v1/projects/:ident/messages/unread` | Get unread messages and advance the read cursor. |

### Skills

| Method | Path | Description |
|--------|------|-------------|
| `PUT` | `/v1/skills/:name` | Upload or replace a skill (raw zip bytes, `Content-Type: application/zip`). |
| `GET` | `/v1/skills` | List all skills. Returns `[{name, size, checksum, uploaded_at}]`. |
| `GET` | `/v1/skills/:name` | Download a skill as a zip file. |
| `DELETE` | `/v1/skills/:name` | Delete a skill. |

### Dashboard

`GET /` — no auth required. HTML page showing project counts, message stats, and skill inventory.

---

## Multi-machine setup

If you have the same project running on multiple machines (e.g. Windows dev box and a Linux build server), point both MCP server instances at the same gateway with the same project identity.

Both instances share:
- The same Discord channel
- The same message history
- The same read cursor (last `get_messages` call from either machine advances it for both)

Skills on the gateway are also accessible from all machines — `agent-sync sync` keeps every machine's skill set up to date.

---

## Troubleshooting

**Bot doesn't receive user messages**
Ensure **Message Content Intent** is enabled in the Discord Developer Portal under your bot's settings. Without it, `message.content` is always empty.

**`get_messages` returns old messages repeatedly**
The read cursor is shared across all agent instances for a project. If a second agent instance calls `get_messages`, it advances the cursor, so the first instance won't see those messages. This is by design for v1.

**Gateway can't create channels**
The bot needs **Manage Channels** permission in your Discord server. Re-invite it using the OAuth2 URL Generator with that permission checked.

**MCP server can't reach the gateway**
Check that `GATEWAY_URL` is correct and the gateway's port (default `7913`) is reachable from the machine running the MCP server. On LAN, ensure your firewall allows the connection.
