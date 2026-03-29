# Gateway Setup (Linux)

## Setup

To set up claude-mail on linux we first have to create the user for the service to run as use the following:

```bash
sudo useradd --system --no-create-home --shell /bin/false cmail
sudo mkdir -p /etc/cmail /var/lib/claude-mail
sudo chown -R cmail:cmail /etc/cmail /var/lib/claude-mail
```

As seen above we also create the var lib directory for app storage.

## Configure the environment file

Create `/etc/cmail/gateway.env` and fill in your values:

```bash
sudo vim /etc/cmail/gateway.env
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
DATABASE_PATH=/var/lib/claude-mail/claude-mail.db

# Delete messages older than N days that are behind the read cursor
MESSAGE_RETENTION_DAYS=30

# Log level: error | warn | info | debug | trace
RUST_LOG=info
```

Restrict permissions so only the service user can read it:

```bash
sudo chown cmail:cmail /etc/cmail/gateway.env
sudo chmod 640 /etc/cmail/gateway.env
```

## Copy the downloaded binaries

Under the releases directory you will now need to download the latest release of claude-mail from the github repository like so:

```bash
wget https://github.com/nitecon/claude-mail/releases/download/v0.1.1/claude-mail-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
tar xf claude-mail-v0.1.1-x86_64-unknown-linux-gnu.tar.gz
sudo cp -f claude-mail-v0.1.1-x86_64-unknown-linux-gnu/claude-mail /usr/local/bin/
sudo cp -f claude-mail-v0.1.1-x86_64-unknown-linux-gnu/claude-mail-gateway /usr/local/bin/
```

## System-D Setup

Now we have to setup the systemd service file with:

```bash
sudo vim /etc/systemd/system/claude-mail-gateway.service
```

Paste the following in to make sure we have a proper service file (adjust to handle your changes...)

```ini
[Unit]
Description=claude-mail Gateway
Documentation=https://github.com/nitecon/claude-mail
After=network.target

[Service]
Type=simple
User=cmail
Group=cmail

EnvironmentFile=/etc/cmail/gateway.env

ExecStart=/usr/local/bin/claude-mail-gateway

Restart=on-failure
RestartSec=5
TimeoutStopSec=10

# Logging goes to the system journal: journalctl -u claude-mail-gateway -f
StandardOutput=journal
StandardError=journal
SyslogIdentifier=claude-mail-gateway

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/claude-mail

[Install]
WantedBy=multi-user.target
```

## Configure systemd

Now we will reload and install it to restart on boot

```bash
sudo systemctl daemon-reload
sudo systemctl enable claude-mail-gateway.service
sudo systemctl restart claude-mail-gateway
```

## Troubleshoot

To validate it is working and to look at the logs run the following:

```bash
journalctl -fu claude-mail-gateway
```
