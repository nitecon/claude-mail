#!/usr/bin/env bash
# Eventic deploy hook for agent-gateway. Invoked from .eventic.yaml on push
# events.
#
# Kept as a standalone bash script (rather than inlined in .eventic.yaml) so:
#   1. We're on bash, not dash — no array/bashism surprises.
#   2. .eventic.yaml never needs to change; edits to this script take effect
#      on the same push (eventic's DiscoverHooks reads .eventic.yaml *before*
#      the git pull, so in-yaml hook changes always lag by one push).
#   3. The hook can be syntax-checked / dry-run locally.
#
# On push to main : build + install binary, flip the per-instance dir
#                   symlink to source-<sha>, retarget /opt/agentic/bin/gateway
#                   at the active instance, restart the dev gateway,
#                   health-check, roll back on failure.
# On push of v*   : same but for prod. Restart is gated on
#                   /etc/eventic/agent-gateway-prod-enabled.
#
# Service resolution: the script prefers the dedicated per-instance units
# `gateway-dev.service` / `gateway-prod.service` if they exist, and falls
# back to the single `gateway.service` while SRE has not yet provisioned a
# dedicated dev gateway. When the split happens, no script change is needed
# — the dedicated units start being used automatically.
#
# Artifact layout under /opt/agentic/bin/:
#   gateway-versions/source-<sha>/gateway   per-commit dev binary
#   gateway-versions/v<tag>/gateway         per-tag prod binary
#   gateway-versions/dev   -> source-<sha>  per-instance dir symlink
#   gateway-versions/prod  -> v<tag>
#   gateway -> gateway-versions/<active>/gateway   (single-service mode)
#
# Retention: 5 newest source-* dirs + 5 newest v* dirs kept; older pruned.

set -euo pipefail

BIN_ROOT="/opt/agentic/bin"
VERSIONS_ROOT="${BIN_ROOT}/gateway-versions"
SVC_USER="agentic"
SVC_GROUP="agentic"

: "${EVENTIC_REF:?EVENTIC_REF must be set (eventic injects this)}"

# Locate a usable cargo (eventic user may not have its own rustup).
CARGO=""
for c in /home/eventic/.cargo/bin/cargo /home/whattingh/.cargo/bin/cargo $(command -v cargo 2>/dev/null); do
  if [ -x "$c" ]; then CARGO="$c"; break; fi
done
[ -n "$CARGO" ] || { echo "[eventic] ERROR: no cargo found in PATH"; exit 1; }

# Determine deploy target from ref.
if echo "${EVENTIC_REF}" | grep -q '^refs/tags/v'; then
  TAG=$(echo "${EVENTIC_REF}" | sed 's|refs/tags/||')   # v1.2.3
  INSTANCE="prod"
  VERSION="${TAG}"
  GATE_FILE="/etc/eventic/agent-gateway-prod-enabled"
elif echo "${EVENTIC_REF}" | grep -q '^refs/heads/main$'; then
  SHA=$(git rev-parse --short=7 HEAD)
  INSTANCE="dev"
  VERSION="source-${SHA}"
  GATE_FILE=""
else
  echo "[eventic] ref=${EVENTIC_REF} is not a deploy trigger (main or v*), skipping"
  exit 0
fi

# Service resolution: prefer dedicated unit, fall back to single 'gateway'
# while SRE has not split dev/prod into separate systemd units.
PREFERRED_SERVICE="gateway-${INSTANCE}"
if systemctl list-unit-files --no-legend "${PREFERRED_SERVICE}.service" 2>/dev/null | grep -q "^${PREFERRED_SERVICE}.service"; then
  SERVICE="$PREFERRED_SERVICE"
else
  echo "[eventic] ${PREFERRED_SERVICE}.service not provisioned — falling back to single 'gateway' unit (single-service mode)"
  SERVICE="gateway"
fi

# AGENT_GATEWAY_VERSION is consumed by crates/gateway/build.rs (strips a
# leading 'v' if present). Source builds get 'main-<sha>'; tag builds get
# the bare tag. Both surface on /version + the dashboard header so the
# running build is always identifiable.
if [ "$INSTANCE" = "prod" ]; then
  export AGENT_GATEWAY_VERSION="${TAG}"
else
  export AGENT_GATEWAY_VERSION="main-${SHA}"
fi

echo "[eventic] Building agent-gateway instance=${INSTANCE} version=${VERSION} (AGENT_GATEWAY_VERSION=${AGENT_GATEWAY_VERSION})..."
"$CARGO" build --release -p gateway

BUILT="./target/release/gateway"
test -x "$BUILT" || { echo "[eventic] ERROR: $BUILT not produced"; exit 1; }

# Smoke test — --version exits 0 via clap before any runtime deps load.
"$BUILT" --version >/dev/null || { echo "[eventic] ERROR: smoke test (--version) failed"; exit 1; }

# Install into a per-version directory under $VERSIONS_ROOT.
VERSION_DIR="${VERSIONS_ROOT}/${VERSION}"
STAGE_DIR="${VERSION_DIR}.tmp"
sudo install -d -m 0775 -o "$SVC_USER" -g "$SVC_GROUP" "$VERSIONS_ROOT"
sudo rm -rf "$STAGE_DIR"
sudo install -d -m 0775 -o "$SVC_USER" -g "$SVC_GROUP" "$STAGE_DIR"
sudo install -m 0775 -o "$SVC_USER" -g "$SVC_GROUP" "$BUILT" "${STAGE_DIR}/gateway"
# Atomic swap (rm-then-mv; same fs so rename is atomic).
[ -d "$VERSION_DIR" ] && sudo rm -rf "$VERSION_DIR"
sudo mv "$STAGE_DIR" "$VERSION_DIR"
echo "[eventic] Installed ${VERSION_DIR}/gateway"

# Capture previous instance-symlink target for rollback.
INSTANCE_LINK="${VERSIONS_ROOT}/${INSTANCE}"
PREV_TARGET=$(readlink "$INSTANCE_LINK" 2>/dev/null || echo "")

# Atomic per-instance dir symlink flip (-n prevents following existing dir symlink).
sudo ln -sfn "$VERSION" "$INSTANCE_LINK"
echo "[eventic] ${INSTANCE_LINK} -> ${VERSION} (previous: ${PREV_TARGET:-<none>})"

# Retention: keep 5 newest source-* and 5 newest v* version dirs.
sudo bash -c "cd '$VERSIONS_ROOT' && ls -1dt source-*/ 2>/dev/null | tail -n +6 | xargs -r rm -rf" || true
sudo bash -c "cd '$VERSIONS_ROOT' && ls -1dt v*/        2>/dev/null | tail -n +6 | xargs -r rm -rf" || true

# Single-service-mode active-binary pointer:
# When SRE splits the units, each unit will have ExecStart pinned to its own
# instance dir and this top-level symlink becomes irrelevant. Today the
# single gateway.service runs whatever /usr/local/bin/gateway resolves to,
# so we always retarget it at the instance we just deployed.
if [ "$SERVICE" = "gateway" ]; then
  sudo ln -sfn "gateway-versions/${INSTANCE}/gateway" "${BIN_ROOT}/gateway"
  echo "[eventic] ${BIN_ROOT}/gateway -> gateway-versions/${INSTANCE}/gateway (single-service mode)"
fi

# Prod gate: stage artifact but do not restart if gate file missing.
if [ "$INSTANCE" = "prod" ] && [ ! -f "$GATE_FILE" ]; then
  echo "[eventic] Prod artifact staged at ${INSTANCE_LINK} -> ${VERSION} but ${SERVICE} restart is GATED OFF"
  echo "[eventic] Enable auto-restart on prod: sudo touch ${GATE_FILE}"
  exit 0
fi

sudo systemctl enable "$SERVICE" 2>/dev/null || true

# Restart and health-check.
echo "[eventic] Restarting ${SERVICE}..."
sudo systemctl restart "$SERVICE"
sleep 3
if ! sudo systemctl is-active --quiet "$SERVICE"; then
  echo "[eventic] ERROR: ${SERVICE} not active after restart — rolling back"
  sudo journalctl -u "$SERVICE" -n 40 --no-pager -p err 2>&1 || true
  if [ -n "$PREV_TARGET" ]; then
    sudo ln -sfn "$PREV_TARGET" "$INSTANCE_LINK"
    if [ "$SERVICE" = "gateway" ]; then
      sudo ln -sfn "gateway-versions/${INSTANCE}/gateway" "${BIN_ROOT}/gateway"
    fi
    sudo systemctl restart "$SERVICE" || true
    echo "[eventic] Rolled back ${INSTANCE_LINK} -> ${PREV_TARGET}"
  fi
  exit 1
fi
echo "[eventic] ${SERVICE} active on ${VERSION}"
