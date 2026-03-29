# claude-mail

Async communication between AI agents and humans via Discord, with cross-machine skill sharing.

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
│   ├── mcp-server/     # stdio MCP server (installed per machine)
│   └── skills-cli/     # CLI for pushing/pulling shared skills
```

---

## Installation

### Pre-built binaries

Each release ships archives for all major platforms. Each archive contains three binaries:
- `claude-mail` — MCP server (per-machine)
- `claude-mail-gateway` — Gateway service
- `claude-mail-skills` — Skills management CLI

**Linux / macOS**

```bash
# Get the latest release tag
VERSION=$(curl -fsSL https://api.github.com/repos/nitecon/claude-mail/releases/latest | grep '"tag_name"' | cut -d'"' -f4)

# Linux x86_64
curl -fsSL "https://github.com/nitecon/claude-mail/releases/download/${VERSION}/claude-mail-${VERSION}-x86_64-unknown-linux-gnu.tar.gz" | tar xz --strip-components=1

# Linux ARM64
curl -fsSL "https://github.com/nitecon/claude-mail/releases/download/${VERSION}/claude-mail-${VERSION}-aarch64-unknown-linux-gnu.tar.gz" | tar xz --strip-components=1

# macOS Apple Silicon
curl -fsSL "https://github.com/nitecon/claude-mail/releases/download/${VERSION}/claude-mail-${VERSION}-aarch64-apple-darwin.tar.gz" | tar xz --strip-components=1

# macOS Intel
curl -fsSL "https://github.com/nitecon/claude-mail/releases/download/${VERSION}/claude-mail-${VERSION}-x86_64-apple-darwin.tar.gz" | tar xz --strip-components=1
```

**Windows (PowerShell)**

```powershell
$version = (Invoke-RestMethod https://api.github.com/repos/nitecon/claude-mail/releases/latest).tag_name
Invoke-WebRequest -Uri "https://github.com/nitecon/claude-mail/releases/download/$version/claude-mail-$version-x86_64-pc-windows-msvc.zip" -OutFile "claude-mail-$version.zip"
Expand-Archive -Path "claude-mail-$version.zip" -DestinationPath "." -Force
```

Move the extracted binaries somewhere on your `PATH` (e.g. `/usr/local/bin` on Linux/macOS, or `C:\Tools` on Windows).

### Build from source

```bash
git clone https://github.com/nitecon/claude-mail
cd claude-mail
cargo build --release
```

Binaries:
- `target/release/gateway`
- `target/release/claude-mail`
- `target/release/claude-mail-skills`

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
DATABASE_PATH=./data/claude-mail.db
MESSAGE_RETENTION_DAYS=30
RUST_LOG=info
```

> `GATEWAY_API_KEY` is the shared secret between the gateway and all clients. Use a long random string (e.g. `openssl rand -hex 32`).

---

### 4. Start the gateway

```bash
./claude-mail-gateway
# or from source: cargo run --release -p gateway
```

You should see:

```
INFO gateway: SQLite database opened at ./data/claude-mail.db
INFO gateway: Gateway listening on http://0.0.0.0:7913
INFO gateway::discord: Discord bot connected as YourBotName#1234
```

The dashboard is available at `http://localhost:7913/` — shows projects, message counts, and skills.

#### Systemd (Linux)

A unit file is included in each release archive:

```bash
sudo cp claude-mail-gateway.service /etc/systemd/system/
sudo mkdir -p /etc/claude-mail
sudo cp crates/gateway/.env.example /etc/claude-mail/gateway.env
# edit /etc/claude-mail/gateway.env with your values
sudo systemctl enable --now claude-mail-gateway
```

---

### 5. Configure the MCP server (interactive)

On each machine where agents will run:

```bash
claude-mail init
```

This prompts for your gateway URL and API key, then writes `~/.claude/claude-mail.conf`.

To add claude-mail to Claude Code:

```bash
claude mcp add claude-mail -- /path/to/claude-mail
# or with an explicit URL override:
claude mcp add claude-mail -- /path/to/claude-mail --url=http://your-gateway:7913
```

---

### 6. Instruct your agent

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

## Skills CLI reference

`claude-mail-skills` manages shared Claude Code skills on the gateway. A skill is any directory containing a `SKILL.md` file.

```bash
# Upload a skill directory to the gateway
claude-mail-skills push ~/.claude/skills/my-skill

# Download a skill from the gateway
claude-mail-skills pull my-skill --to ~/.claude/skills

# List all skills on the gateway
claude-mail-skills list

# Delete a skill from the gateway
claude-mail-skills delete my-skill

# Bidirectional sync: push new/changed local, pull new remote
claude-mail-skills sync --dir ~/.claude/skills
```

Configuration uses the same `~/.claude/claude-mail.conf` written by `claude-mail init`, or CLI flags:

```bash
claude-mail-skills --url http://your-gateway:7913 --api-key <key> list
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

Skills on the gateway are also accessible from all machines — `claude-mail-skills sync` keeps every machine's skill set up to date.

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
