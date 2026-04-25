#!/usr/bin/env bash
set -euo pipefail

REPO="nitecon/agent-gateway"
INSTALL_DIR="/opt/agentic/bin"
BINARY_NAME="gateway"
SYMLINK="/usr/local/bin/gateway"
SVC_USER="agentic"
SVC_GROUP="agentic"
CONFIG_DIR="/etc/agent-gateway"
DATA_DIR="/opt/agentic/gateway"
SERVICE_NAME="gateway.service"

# --- Helpers ----------------------------------------------------------------

info()  { printf '\033[1;32m[INFO]\033[0m  %s\n' "$*"; }
warn()  { printf '\033[1;33m[WARN]\033[0m  %s\n' "$*"; }
error() { printf '\033[1;31m[ERROR]\033[0m %s\n' "$*" >&2; exit 1; }

# --- Pre-flight checks ------------------------------------------------------

if [ "$(id -u)" -ne 0 ]; then
  error "This script must be run as root. Try: curl -fsSL <url> | sudo bash"
fi

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  linux)  PLATFORM="linux" ;;
  *)      error "Gateway server setup is only supported on Linux. Got: $OS" ;;
esac

case "$ARCH" in
  x86_64)        ARCH="x86_64" ;;
  aarch64|arm64) ARCH="aarch64" ;;
  *)             error "Unsupported architecture: $ARCH" ;;
esac

# --- Create agentic system user and group -----------------------------------

if ! getent group "$SVC_GROUP" >/dev/null 2>&1; then
  groupadd --system "$SVC_GROUP"
  info "Created system group: ${SVC_GROUP}"
fi

if ! getent passwd "$SVC_USER" >/dev/null 2>&1; then
  useradd --system --gid "$SVC_GROUP" --no-create-home --shell /usr/sbin/nologin "$SVC_USER"
  info "Created system user: ${SVC_USER}"
fi

# Add all human users (uid >= 1000, excluding nobody) to the agentic group
while IFS=: read -r username _ uid _; do
  if [ "$uid" -ge 1000 ] 2>/dev/null && [ "$username" != "nobody" ]; then
    if ! id -nG "$username" 2>/dev/null | grep -qw "$SVC_GROUP"; then
      usermod -aG "$SVC_GROUP" "$username"
      info "Added user ${username} to ${SVC_GROUP} group"
    fi
  fi
done < /etc/passwd

# --- Set /opt/agentic ownership ---------------------------------------------

mkdir -p /opt/agentic/bin "$DATA_DIR"
chown -R "${SVC_USER}:${SVC_GROUP}" /opt/agentic
chmod -R 775 /opt/agentic
info "Set /opt/agentic ownership to ${SVC_USER}:${SVC_GROUP}"

# --- Resolve latest version -------------------------------------------------

info "Resolving latest release..."
if command -v curl &>/dev/null; then
  DOWNLOAD="curl -fsSL"
  DOWNLOAD_OUT="curl -fsSL -o"
elif command -v wget &>/dev/null; then
  DOWNLOAD="wget -qO-"
  DOWNLOAD_OUT="wget -qO"
else
  error "Neither curl nor wget found. Install one and retry."
fi

# Resolve via redirect header — avoids GitHub API rate limits entirely
LATEST_TAG=$(curl -sI "https://github.com/${REPO}/releases/latest" \
  | grep -i '^location:' | sed -E 's|.*/tag/([^ \r]+).*|\1|' | tr -d '[:space:]') || true

if [ -z "$LATEST_TAG" ]; then
  error "Could not determine latest release from GitHub."
fi

info "Latest version: ${LATEST_TAG}"

ARCHIVE_NAME="agent-gateway-${LATEST_TAG}-${PLATFORM}-${ARCH}.tar.gz"
DOWNLOAD_URL="https://github.com/${REPO}/releases/download/${LATEST_TAG}/${ARCHIVE_NAME}"

# --- Check existing installation --------------------------------------------

if [ -f "${INSTALL_DIR}/${BINARY_NAME}" ]; then
  CURRENT_VERSION=$(${INSTALL_DIR}/${BINARY_NAME} --version 2>/dev/null || echo "unknown")
  info "Existing installation found: ${CURRENT_VERSION}"
  info "Upgrading to ${LATEST_TAG}..."
else
  info "No existing installation found. Installing fresh."
fi

# --- Download and extract ---------------------------------------------------

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

info "Downloading ${ARCHIVE_NAME}..."
$DOWNLOAD_OUT "${TMPDIR}/${ARCHIVE_NAME}" "$DOWNLOAD_URL"

info "Extracting..."
tar xzf "${TMPDIR}/${ARCHIVE_NAME}" -C "$TMPDIR"

# --- Install binary ---------------------------------------------------------

BIN_PATH=$(find "$TMPDIR" -name "$BINARY_NAME" -type f ! -name "*.tar.gz" ! -name "*.service" | head -1)
if [ -z "$BIN_PATH" ]; then
  error "Binary '${BINARY_NAME}' not found in archive."
fi

mv "$BIN_PATH" "${INSTALL_DIR}/${BINARY_NAME}"
chown "${SVC_USER}:${SVC_GROUP}" "${INSTALL_DIR}/${BINARY_NAME}"
chmod 775 "${INSTALL_DIR}/${BINARY_NAME}"
ln -sf "${INSTALL_DIR}/${BINARY_NAME}" "$SYMLINK"
info "Installed ${INSTALL_DIR}/${BINARY_NAME} (symlinked to ${SYMLINK})"

# --- Create config directory ------------------------------------------------

mkdir -p "$CONFIG_DIR"
chown root:"$SVC_GROUP" "$CONFIG_DIR"
chmod 750 "$CONFIG_DIR"
info "Config directory: ${CONFIG_DIR}"

# --- Install systemd service (embedded, always up to date) ------------------

cat > "/etc/systemd/system/${SERVICE_NAME}" <<'SVCEOF'
[Unit]
Description=agent-gateway
Documentation=https://github.com/nitecon/agent-gateway
After=network.target

[Service]
Type=simple
User=agentic
Group=agentic

EnvironmentFile=/etc/agent-gateway/gateway.env

# Deploys are owned by Eventic (see .eventic.yaml + deploy/eventic-deploy.sh).
# The unit just runs whatever binary the symlink currently resolves to — no
# self-update on start.
ExecStart=/usr/local/bin/gateway

Restart=on-failure
RestartSec=5
TimeoutStopSec=10

StandardOutput=journal
StandardError=journal
SyslogIdentifier=agent-gateway

# Hardening
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/opt/agentic

[Install]
WantedBy=multi-user.target
SVCEOF
info "Installed systemd service: /etc/systemd/system/${SERVICE_NAME}"

# --- Install template env file (don't overwrite existing) -------------------

if [ ! -f "${CONFIG_DIR}/gateway.env" ]; then
  cat > "${CONFIG_DIR}/gateway.env" <<'ENVEOF'
# agent-gateway Configuration
# Fill in the required values and uncomment as needed.

GATEWAY_API_KEY=CHANGE_ME
GATEWAY_HOST=0.0.0.0
GATEWAY_PORT=7913

# Discord integration
# DISCORD_BOT_TOKEN=
# DISCORD_GUILD_ID=
# DISCORD_CATEGORY_ID=

# Database
DATABASE_PATH=/opt/agentic/gateway/agent-gateway.db

# Message retention (days)
MESSAGE_RETENTION_DAYS=30

# Default channel plugin
DEFAULT_CHANNEL=discord
ENVEOF
  chown root:"$SVC_GROUP" "${CONFIG_DIR}/gateway.env"
  chmod 640 "${CONFIG_DIR}/gateway.env"
  info "Created template config: ${CONFIG_DIR}/gateway.env"
else
  # Migrate DATABASE_PATH from old location if still pointing there
  if grep -q '/var/lib/agent-gateway' "${CONFIG_DIR}/gateway.env" 2>/dev/null; then
    sed -i 's|/var/lib/agent-gateway|/opt/agentic/gateway|g' "${CONFIG_DIR}/gateway.env"
    info "Updated DATABASE_PATH in existing config to /opt/agentic/gateway"
  fi
  info "Existing config preserved: ${CONFIG_DIR}/gateway.env"
fi

# --- Migrate old data directory if present ----------------------------------

OLD_DATA_DIR="/var/lib/agent-gateway"
if [ -d "$OLD_DATA_DIR" ] && [ "$(ls -A "$OLD_DATA_DIR" 2>/dev/null)" ]; then
  info "Migrating data from ${OLD_DATA_DIR} to ${DATA_DIR}..."
  cp -a "${OLD_DATA_DIR}/." "${DATA_DIR}/"
  chown -R "${SVC_USER}:${SVC_GROUP}" "${DATA_DIR}"
  info "Migration complete. Old directory preserved at ${OLD_DATA_DIR} (safe to remove)"
fi

# --- Reload systemd ---------------------------------------------------------

systemctl daemon-reload
info "Reloaded systemd daemon"

# --- Done -------------------------------------------------------------------

echo ""
info "Gateway installation complete!"
echo ""
echo "  Binary:   ${INSTALL_DIR}/${BINARY_NAME}"
echo "  Symlink:  ${SYMLINK}"
echo "  Service:  /etc/systemd/system/${SERVICE_NAME}"
echo "  Config:   ${CONFIG_DIR}/gateway.env"
echo "  Data:     ${DATA_DIR}/"
echo "  Version:  ${LATEST_TAG}"
echo ""
echo "Next steps:"
echo "  1. Edit ${CONFIG_DIR}/gateway.env and set your API key and Discord tokens"
echo "  2. Enable and start the service:"
echo "     systemctl enable --now gateway"
echo "  3. Check status:"
echo "     systemctl status gateway"
echo "     journalctl -u gateway -f"
echo ""
