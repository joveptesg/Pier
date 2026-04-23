#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Pier PaaS — Install Script
# Installs Pier as a native systemd service on Linux (Ubuntu/Debian/RHEL)
# Usage:
#   sudo bash install.sh --binary /path/to/pier
#   sudo bash install.sh --binary ./target/release/pier
# ============================================================================

PIER_USER="pier"
PIER_DIR="/opt/pier"
PIER_BIN="${PIER_DIR}/bin/pier"
PIER_DATA="${PIER_DIR}/data"
PIER_ENV="${PIER_DIR}/.env"
PIER_SERVICE="/etc/systemd/system/pier.service"
PIER_PORT=8443

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }

# ── Parse arguments ──────────────────────────────────────────────────────────

BINARY_PATH=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)
            BINARY_PATH="$2"
            shift 2
            ;;
        --port)
            PIER_PORT="$2"
            shift 2
            ;;
        --help|-h)
            echo "Usage: sudo bash install.sh --binary /path/to/pier [--port 8443]"
            echo ""
            echo "Options:"
            echo "  --binary PATH   Path to compiled pier binary (required)"
            echo "  --port PORT     HTTP listen port (default: 8443)"
            exit 0
            ;;
        *)
            error "Unknown option: $1. Use --help for usage."
            ;;
    esac
done

[[ -z "$BINARY_PATH" ]] && error "Missing --binary argument. Usage: sudo bash install.sh --binary /path/to/pier"
[[ ! -f "$BINARY_PATH" ]] && error "Binary not found: $BINARY_PATH"

# ── Check root ───────────────────────────────────────────────────────────────

[[ $EUID -ne 0 ]] && error "This script must be run as root (sudo)"

# ── Check prerequisites ─────────────────────────────────────────────────────

info "Checking prerequisites..."

if ! command -v docker &>/dev/null; then
    error "Docker is not installed. Install Docker first: https://docs.docker.com/engine/install/"
fi

if ! docker info &>/dev/null; then
    error "Docker daemon is not running. Start it: systemctl start docker"
fi

if ! docker compose version &>/dev/null; then
    error "Docker Compose plugin not found. Install: https://docs.docker.com/compose/install/"
fi

if ! command -v git &>/dev/null; then
    error "git is not installed. Install: apt install git / yum install git"
fi

info "All prerequisites OK: Docker $(docker --version | grep -oP '\d+\.\d+\.\d+'), Compose $(docker compose version --short), git $(git --version | grep -oP '\d+\.\d+\.\d+')"

# ── Check if upgrading ───────────────────────────────────────────────────────

UPGRADE=false
if systemctl is-active --quiet pier 2>/dev/null; then
    UPGRADE=true
    info "Existing Pier installation detected — upgrading..."
    systemctl stop pier
fi

# ── Create user ──────────────────────────────────────────────────────────────

if ! id "$PIER_USER" &>/dev/null; then
    info "Creating user: $PIER_USER"
    useradd --system --no-create-home --shell /usr/sbin/nologin "$PIER_USER"
fi

# Add pier user to docker group
if ! groups "$PIER_USER" | grep -q docker; then
    info "Adding $PIER_USER to docker group"
    usermod -aG docker "$PIER_USER"
fi

# ── Create directories ───────────────────────────────────────────────────────

info "Creating directories..."
mkdir -p "${PIER_DIR}/bin"
mkdir -p "${PIER_DATA}/stacks"
mkdir -p "${PIER_DATA}/traefik/dynamic"
mkdir -p "${PIER_DATA}/tmp"
mkdir -p "${PIER_DIR}/.docker"

# Ensure /root/.docker exists before pier starts so the systemd unit's
# BindReadOnlyPaths=-/root/.docker actually creates the mount; otherwise
# the first `docker login` after install would require `systemctl restart pier`.
mkdir -p /root/.docker
chmod 700 /root/.docker

# ── Install binary ───────────────────────────────────────────────────────────

info "Installing binary to ${PIER_BIN}"
cp "$BINARY_PATH" "$PIER_BIN"
chmod 755 "$PIER_BIN"

# ── Create .env (only if not exists — preserve existing config on upgrade) ───

if [[ ! -f "$PIER_ENV" ]]; then
    info "Creating ${PIER_ENV}"
    cat > "$PIER_ENV" <<EOF
PIER_HOST=0.0.0.0
PIER_PORT=${PIER_PORT}
PIER_DATA_DIR=${PIER_DATA}
PIER_LOG_LEVEL=info
PIER_PORT_RANGE_START=10000
PIER_PORT_RANGE_END=65000
EOF
else
    info "Preserving existing ${PIER_ENV}"
fi

# ── Ensure stable PIER_SECRET is set in .env ────────────────────────────────
# Systemd `ReadWritePaths=...` blocks the runtime from writing to /opt/pier/.env,
# so the secret MUST be generated here, pre-start, while we're still root.

if ! grep -q '^PIER_SECRET=' "$PIER_ENV" 2>/dev/null; then
    info "Generating stable PIER_SECRET"
    SECRET=$(head -c 32 /dev/urandom | base64)
    echo "PIER_SECRET=${SECRET}" >> "$PIER_ENV"
fi
chmod 600 "$PIER_ENV"

# ── Harvest historical PIER_SECRET values from journald for auto-recovery ──
# Earlier Pier versions generated a fresh random key on every call because
# the systemd unit denied writes to /opt/pier/.env. Each of those keys was
# logged as a WARN line in journald. We dump them into a recovery file so
# the binary can try each one against encrypted env_json rows on startup.

RECOVERY_FILE="${PIER_DATA}/.pier-recovery-keys"
if command -v journalctl &>/dev/null; then
    journalctl -u pier --no-pager 2>/dev/null \
        | grep -oE 'PIER_SECRET=[A-Za-z0-9+/=]+' \
        | sed 's/^PIER_SECRET=//' \
        | sort -u > "$RECOVERY_FILE" || true
    if [[ -s "$RECOVERY_FILE" ]]; then
        KEYCOUNT=$(wc -l < "$RECOVERY_FILE")
        info "Collected ${KEYCOUNT} historical keys for env_json recovery"
    else
        rm -f "$RECOVERY_FILE"
    fi
fi

# ── Set ownership ────────────────────────────────────────────────────────────

chown -R "$PIER_USER":"$PIER_USER" "$PIER_DIR"

# ── Install systemd unit ─────────────────────────────────────────────────────

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ -f "${SCRIPT_DIR}/pier.service" ]]; then
    info "Installing systemd unit from ${SCRIPT_DIR}/pier.service"
    cp "${SCRIPT_DIR}/pier.service" "$PIER_SERVICE"
else
    info "Generating systemd unit"
    cat > "$PIER_SERVICE" <<EOF
[Unit]
Description=Pier PaaS
After=network.target docker.service
Requires=docker.service

[Service]
Type=simple
User=${PIER_USER}
Group=docker
WorkingDirectory=${PIER_DIR}
EnvironmentFile=${PIER_ENV}
ExecStart=${PIER_BIN}
Restart=on-failure
RestartSec=5

# Security hardening
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=${PIER_DATA} ${PIER_DIR}/.docker /tmp
ProtectHome=true
BindReadOnlyPaths=-/root/.docker
Environment=HOME=${PIER_DIR}
Environment=DOCKER_CONFIG=/root/.docker
Environment=GIT_CONFIG_NOSYSTEM=1

# Logging
StandardOutput=journal
StandardError=journal
SyslogIdentifier=pier

[Install]
WantedBy=multi-user.target
EOF
fi

# ── Enable and start ─────────────────────────────────────────────────────────

systemctl daemon-reload
systemctl enable pier

if [[ "$UPGRADE" == true ]]; then
    systemctl start pier
    info "Pier upgraded and restarted"
else
    systemctl start pier
    info "Pier installed and started"
fi

# ── Wait for startup ─────────────────────────────────────────────────────────

sleep 2

if systemctl is-active --quiet pier; then
    # Detect public IP for display
    PUBLIC_IP=$(curl -s --max-time 5 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')

    echo ""
    echo -e "${GREEN}════════════════════════════════════════════════════════════${NC}"
    echo -e "${GREEN}  Pier PaaS installed successfully!${NC}"
    echo -e "${GREEN}════════════════════════════════════════════════════════════${NC}"
    echo ""
    echo -e "  Dashboard:  ${YELLOW}http://${PUBLIC_IP}:${PIER_PORT}${NC}"
    echo -e "  Setup:      ${YELLOW}http://${PUBLIC_IP}:${PIER_PORT}/setup${NC}"
    echo ""
    echo -e "  Logs:       journalctl -u pier -f"
    echo -e "  Status:     systemctl status pier"
    echo -e "  Config:     ${PIER_ENV}"
    echo -e "  Data:       ${PIER_DATA}"
    echo ""
    echo -e "  ${GREEN}Visit /setup to create your admin account.${NC}"
    echo ""
else
    echo ""
    error "Pier failed to start. Check logs: journalctl -u pier --no-pager -n 50"
fi
