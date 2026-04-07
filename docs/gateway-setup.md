# Gateway Setup (Linux)

## Install

The install script downloads the latest release and places the gateway binary in `/opt/agentic/bin/`:

```bash
curl -fsSL https://raw.githubusercontent.com/nitecon/agent-gateway/main/install-gateway.sh | sudo bash
```

## Create the service user and directories

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin agentic
sudo mkdir -p /etc/agent-gateway /var/lib/agent-gateway
sudo chown root:agentic /etc/agent-gateway
sudo chown agentic:agentic /var/lib/agent-gateway
```

## Configure the environment file

Create `/etc/agent-gateway/gateway.env` and fill in your values:

```bash
sudo vim /etc/agent-gateway/gateway.env
```

Paste and edit the following (a full reference is also at `.env.example` in the repository root):

```ini
# Discord bot token from https://discord.com/developers/applications
DISCORD_BOT_TOKEN=

# The Guild (server) ID where project channels will be created
DISCORD_GUILD_ID=

# Optional: category channel ID to group project channels under
DISCORD_CATEGORY_ID=

# Shared secret — MCP clients must send this in Authorization: Bearer <key>
GATEWAY_API_KEY=your-secret-key-here

# HTTP listen config
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913

# SQLite database path
DATABASE_PATH=/var/lib/agent-gateway/agent-gateway.db

# Delete messages older than N days that are behind the read cursor
MESSAGE_RETENTION_DAYS=30

# Log level: error | warn | info | debug | trace
RUST_LOG=info
```

Restrict permissions so only the service user can read it:

```bash
sudo chown root:agentic /etc/agent-gateway/gateway.env
sudo chmod 640 /etc/agent-gateway/gateway.env
```

## Systemd Setup

Create the service file:

```bash
sudo vim /etc/systemd/system/gateway.service
```

Paste the following:

```ini
[Unit]
Description=agent-gateway
Documentation=https://github.com/nitecon/agent-gateway
After=network.target

[Service]
Type=simple
User=agentic
Group=agentic

# Secrets and config live in /etc/agent-gateway/gateway.env (not world-readable).
# Copy crates/gateway/.env.example to /etc/agent-gateway/gateway.env and fill in values.
EnvironmentFile=/etc/agent-gateway/gateway.env

ExecStartPre=/usr/local/bin/gateway update
ExecStart=/usr/local/bin/gateway

Restart=on-failure
RestartSec=5
TimeoutStopSec=10

# Logging goes to the system journal: journalctl -u gateway -f
StandardOutput=journal
StandardError=journal
SyslogIdentifier=agent-gateway

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/agent-gateway /opt/agentic/bin

[Install]
WantedBy=multi-user.target
```

The `ExecStartPre` line runs `gateway update` before every start, which automatically downloads the latest binaries.

## Enable and start the service

```bash
sudo systemctl daemon-reload
sudo systemctl enable gateway.service
sudo systemctl start gateway
```

## Troubleshoot

To validate it is working and to look at the logs:

```bash
journalctl -fu gateway
```
