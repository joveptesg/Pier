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

# ── Ensure baseline swap (floor) — runtime / buildkit safety net ─────────────
# NOTE: this does NOT fix a build OOM. install.sh installs a PRE-BUILT binary
# and runs AFTER compilation, so it cannot rescue a `cargo build` that already
# got SIGKILL'd — that's build-from-source.sh's job (swap BEFORE the build).
# Here we only guarantee a 4 GiB swap floor so the running host (pier + buildkit
# image builds) has an OOM relief valve. Opt out with PIER_SKIP_SWAP=1.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
if [[ "${PIER_SKIP_SWAP:-0}" != "1" ]]; then
    if [[ -f "${SCRIPT_DIR}/lib-swap.sh" ]]; then
        # shellcheck source=lib-swap.sh
        source "${SCRIPT_DIR}/lib-swap.sh"
        ensure_swap 4096 4096 || warn "swap-страховка пропущена (см. сообщение выше)"
    else
        warn "lib-swap.sh не найден рядом с install.sh — пропускаю настройку swap."
    fi
fi

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

if ! command -v curl &>/dev/null; then
    error "curl is not installed. Install: apt install curl / yum install curl"
fi

info "All prerequisites OK: Docker $(docker --version | grep -oP '\d+\.\d+\.\d+'), Compose $(docker compose version --short), git $(git --version | grep -oP '\d+\.\d+\.\d+')"

# ── Railpack auto-build (optional) ───────────────────────────────────────────
# Provisions the railpack CLI + a moby/buildkit daemon container so that the
# "Auto-build (Railpack)" application source in the UI can build images
# directly from source without a Dockerfile. Skip with PIER_SKIP_RAILPACK=1
# on hosts where you don't need the feature (saves ~150MB RAM idle).
#
# Resource cap: the buildkit container is launched with --memory=$BUILDKIT_MEM
# (default 4g) to keep multi-GB Node/Python/Rust builds from OOM-killing
# the host. Override via PIER_BUILDKIT_MEMORY=... before re-running install.sh.

if [[ "${PIER_SKIP_RAILPACK:-0}" != "1" ]]; then
    if ! command -v railpack &>/dev/null; then
        info "Installing railpack CLI (Railway's zero-config builder)..."
        # Pull the latest release tag from GitHub then download the matching
        # linux-amd64 binary. Failure is non-fatal — Pier still works for
        # Dockerfile / Compose / Docker Image deploys, only the "Auto-build"
        # catalog item will be unusable.
        RAILPACK_TAG=$(curl -fsSL https://api.github.com/repos/railwayapp/railpack/releases/latest 2>/dev/null \
            | grep -oP '"tag_name":\s*"\K[^"]+' | head -n1)
        if [[ -n "$RAILPACK_TAG" ]]; then
            # railpack ships a versioned musl tarball (the old flat
            # railpack-linux-amd64 asset name 404s on current releases).
            if curl -fsSL --retry 5 --retry-all-errors -o /tmp/railpack.tgz \
                "https://github.com/railwayapp/railpack/releases/download/${RAILPACK_TAG}/railpack-${RAILPACK_TAG}-x86_64-unknown-linux-musl.tar.gz" 2>/dev/null \
                && tar xzf /tmp/railpack.tgz -C /tmp 2>/dev/null \
                && install -m755 "$(find /tmp -maxdepth 2 -name railpack -type f | head -n1)" /usr/local/bin/railpack; then
                info "Installed railpack $RAILPACK_TAG"
            else
                warn "Failed to download railpack binary — Auto-build will be unavailable"
            fi
        else
            warn "Could not query GitHub for the latest railpack release — Auto-build will be unavailable"
        fi
    else
        info "railpack already installed: $(railpack --version 2>/dev/null | head -n1)"
    fi

    if command -v railpack &>/dev/null; then
        BUILDKIT_MEM="${PIER_BUILDKIT_MEMORY:-4g}"
        if ! docker ps --format '{{.Names}}' | grep -q '^buildkit$'; then
            info "Starting moby/buildkit container (memory cap: $BUILDKIT_MEM)..."
            docker run -d --name buildkit \
                --privileged \
                --restart=unless-stopped \
                --memory="$BUILDKIT_MEM" \
                --memory-swap="$BUILDKIT_MEM" \
                moby/buildkit:latest >/dev/null 2>&1 \
                || warn "Failed to start buildkit container — Auto-build will be unavailable until manual fix"
        else
            info "buildkit container already running"
        fi
    fi
fi

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

# Bind-mount target for /root/.docker — ProtectHome=true makes /root inaccessible
# even with BindReadOnlyPaths, so we mount the host's docker config under
# /opt/pier/host-docker (outside ProtectHome's scope, read-only).
# DOCKER_CONFIG points to /opt/pier/.docker (writable); we symlink config.json
# from there into the read-only bind so docker CLI subcommands (buildx,
# contexts, plugins) can write state without fighting the read-only auth source.
mkdir -p "${PIER_DIR}/host-docker"
ln -sfn "${PIER_DIR}/host-docker/config.json" "${PIER_DIR}/.docker/config.json"

# Ensure /root/.docker exists pre-start so the optional bind always has a source;
# any future `docker login` updates the same file the bind exposes to pier.
mkdir -p /root/.docker
chmod 700 /root/.docker

# Grant pier user read access to /root/.docker via ACL. docker login writes
# config.json with mode 600 (owner-only), so the docker group membership of
# pier doesn't help; we need an explicit user ACL. Default ACL ensures new
# config.json files (after PAT rotation) inherit the permission.
if ! command -v setfacl &>/dev/null; then
    info "Installing 'acl' package (needed for persistent pier read access to /root/.docker)"
    if command -v apt-get &>/dev/null; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y acl >/dev/null 2>&1 \
            || warn "apt-get install acl failed"
    elif command -v dnf &>/dev/null; then
        dnf install -y acl >/dev/null 2>&1 || warn "dnf install acl failed"
    elif command -v yum &>/dev/null; then
        yum install -y acl >/dev/null 2>&1 || warn "yum install acl failed"
    elif command -v apk &>/dev/null; then
        apk add --no-cache acl >/dev/null 2>&1 || warn "apk add acl failed"
    fi
fi

if command -v setfacl &>/dev/null; then
    setfacl -m u:"$PIER_USER":rx /root/.docker || warn "setfacl on /root/.docker failed — pier may not see host docker creds"
    setfacl -d -m u:"$PIER_USER":r /root/.docker || true
    [[ -f /root/.docker/config.json ]] && setfacl -m u:"$PIER_USER":r /root/.docker/config.json || true
else
    [[ -f /root/.docker/config.json ]] && chmod 644 /root/.docker/config.json || true
    warn "setfacl not found — пакет 'acl' не установлен и не удалось поставить автоматически."
    warn "Сейчас сработал fallback chmod 644 — это работает разово, но СЛЕДУЮЩИЙ"
    warn "'docker login' сбросит права обратно на 600 и Pier перестанет видеть config.json."
    warn ""
    warn "Для надёжности (особенно при ротации PAT каждые 90 дней) — установи acl"
    warn "и перезапусти install:"
    warn "    apt install -y acl     # или dnf/yum/apk install acl"
    warn "    sudo bash $(realpath "$0") --binary $BINARY_PATH"
    warn ""
    warn "После этого default ACL на /root/.docker будет наследоваться любыми"
    warn "будущими config.json автоматически."
fi

# ── Install WireGuard tools (for the optional mesh) ──────────────────────────
# pier-net-helper runs sandboxed (ProtectSystem=strict) and CANNOT apt-get, so
# the WireGuard CLI must be present on the host before any mesh op runs. Install
# it here (best-effort — mesh is opt-in; non-fatal if the package is missing).
if ! command -v wg &>/dev/null; then
    info "Installing WireGuard tools..."
    if command -v apt-get &>/dev/null; then
        DEBIAN_FRONTEND=noninteractive apt-get install -y wireguard wireguard-tools >/dev/null 2>&1 \
            || warn "apt-get install wireguard failed — mesh unavailable until installed"
    elif command -v dnf &>/dev/null; then
        dnf install -y wireguard-tools >/dev/null 2>&1 || warn "dnf install wireguard-tools failed"
    elif command -v yum &>/dev/null; then
        yum install -y wireguard-tools >/dev/null 2>&1 || warn "yum install wireguard-tools failed"
    fi
fi
# The helper writes wg0.conf + wg0.privkey here; systemd ReadWritePaths only
# makes the path writable if it EXISTS when the unit starts.
mkdir -p /etc/wireguard && chmod 700 /etc/wireguard

# ── Provision the local pier-net-helper (privileged mesh helper) ─────────────
# The core drives its OWN mesh node through this helper over a unix socket
# (/run/pier/net.sock). Dormant until the operator enables the mesh in the UI.
# Prefer a binary shipped next to the core binary; otherwise pull from release.
HELPER_BIN=/usr/local/bin/pier-net-helper
HELPER_SRC="$(dirname "$BINARY_PATH")/pier-net-helper"
if [[ -f "$HELPER_SRC" ]]; then
    install -m755 "$HELPER_SRC" "$HELPER_BIN"
    info "Installed pier-net-helper from $HELPER_SRC"
elif curl -fsSL -o "$HELPER_BIN" \
        "https://github.com/joveptesg/Pier/releases/download/latest/pier-net-helper-linux-amd64" 2>/dev/null; then
    chmod 0755 "$HELPER_BIN"
    info "Installed pier-net-helper from release"
else
    warn "pier-net-helper unavailable — WireGuard mesh features will be disabled"
fi
if [[ -x "$HELPER_BIN" ]]; then
    cat > /etc/systemd/system/pier-net-helper.service <<HELPER_UNIT
[Unit]
Description=Pier Network Helper (privileged WireGuard mesh operations)
After=network-pre.target
Before=pier.service

[Service]
Type=simple
ExecStart=${HELPER_BIN}
Restart=on-failure
RestartSec=2
User=root
# Group=pier so /run/pier/net.sock is created root:pier and pier-core (running
# as the pier user) can reach it.
Group=pier
RuntimeDirectory=pier
RuntimeDirectoryMode=0750
ProtectSystem=strict
ReadWritePaths=-/etc/wireguard /run/pier
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
SystemCallArchitectures=native
AmbientCapabilities=CAP_NET_ADMIN CAP_SYS_MODULE
CapabilityBoundingSet=CAP_NET_ADMIN CAP_SYS_MODULE
HELPER_UNIT
    chmod 644 /etc/systemd/system/pier-net-helper.service
    systemctl daemon-reload
    systemctl enable --now pier-net-helper.service \
        || warn "pier-net-helper failed to start; mesh features unavailable"
fi

# ── Install binary ───────────────────────────────────────────────────────────

info "Installing binary to ${PIER_BIN}"
cp "$BINARY_PATH" "$PIER_BIN"
chmod 755 "$PIER_BIN"

# Stage agent binaries next to the core so it can SERVE them to enrolling agents
# (GET /api/v1/servers/download/{name}). Required because the repo is private —
# agents cannot fetch pier-agent/pier-net-helper from GitHub releases. The
# enrollment install_script downloads them from the core instead.
BIN_DIR="$(dirname "$PIER_BIN")"
for _b in pier-agent pier-net-helper; do
    if [[ -f "$(dirname "$BINARY_PATH")/$_b" ]]; then
        install -m755 "$(dirname "$BINARY_PATH")/$_b" "$BIN_DIR/$_b"
        info "Staged $_b for agent enrollment"
    else
        warn "$_b not found next to core binary — agent enrollment of $_b will fail"
    fi
done

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

# ── Generate /setup bootstrap token (only when no users yet) ────────────────
# Until the first admin exists, `/setup` is reachable by anyone who can hit
# the panel port. The token closes that public window: pier-core checks the
# query string against this file and 404's anything else. pier-core deletes
# the file in two cases: (a) successful first-admin creation via consume(),
# and (b) at startup when the users table is non-empty (stale leftover from
# a legacy install). After pier has booted, the file's presence is therefore
# the canonical "admin not yet created" signal — checked further down.

SETUP_TOKEN_FILE="${PIER_DATA}/.setup_token"

# Truly fresh install ⇔ no DB on disk yet. Only then do we mint a new token
# before starting pier; otherwise we leave any leftover token alone and let
# pier-core's startup cleanup decide whether to keep or remove it.
if [[ ! -f "${PIER_DATA}/pier.db" && ! -f "$SETUP_TOKEN_FILE" ]]; then
    info "Generating /setup bootstrap token"
    head -c 32 /dev/urandom | base64 | tr '+/' '-_' | tr -d '=' > "$SETUP_TOKEN_FILE"
    chmod 400 "$SETUP_TOKEN_FILE"
fi

# ── Set ownership ────────────────────────────────────────────────────────────

chown -R "$PIER_USER":"$PIER_USER" "$PIER_DIR"

# Re-tighten setup token after recursive chown (chown -R restores 644 default).
if [[ -f "$SETUP_TOKEN_FILE" ]]; then
    chmod 400 "$SETUP_TOKEN_FILE"
fi

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
# `pier` group lets pier-core reach the pier-net-helper socket
# (/run/pier/net.sock, 0660 root:pier) for local WireGuard mesh ops.
SupplementaryGroups=systemd-journal adm pier
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
BindReadOnlyPaths=-/root/.docker:${PIER_DIR}/host-docker
Environment=HOME=${PIER_DIR}
Environment=DOCKER_CONFIG=${PIER_DIR}/.docker
Environment=GIT_CONFIG_NOSYSTEM=1
Environment=BUILDKIT_HOST=docker-container://buildkit

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
# Poll the HTTPS port instead of relying on `systemctl is-active`, which
# returns true the moment the process is spawned — before pier finishes
# migrations, stale-token cleanup, and binding the listener. We need the
# listener up so the `.setup_token` file state below is authoritative.

PIER_READY=false
for _ in $(seq 1 30); do
    if curl -sk --max-time 1 -o /dev/null "https://127.0.0.1:${PIER_PORT}/"; then
        PIER_READY=true
        break
    fi
    sleep 1
done

if [[ "$PIER_READY" != true ]]; then
    if ! systemctl is-active --quiet pier; then
        echo ""
        error "Pier failed to start. Check logs: journalctl -u pier --no-pager -n 50"
    fi
    warn "Pier process is running but did not respond on :${PIER_PORT} within 30s — continuing anyway"
fi

# ── Firewall (ufw) ───────────────────────────────────────────────────────────
# Lock the host down to the ports Pier needs. Opt out with PIER_SKIP_FIREWALL=1
# (e.g. if you manage nftables yourself). The live SSH port is detected and
# allowed FIRST so enabling ufw can't lock you out.
if [[ "${PIER_SKIP_FIREWALL:-0}" != "1" ]]; then
    if command -v ufw &>/dev/null; then
        SSH_PORT=$(sshd -T 2>/dev/null | awk '/^port /{print $2; exit}')
        SSH_PORT=${SSH_PORT:-22}
        info "Configuring firewall (ufw): SSH ${SSH_PORT}, ${PIER_PORT}/80/443 tcp, 51820/udp"
        ufw allow "${SSH_PORT}"/tcp  >/dev/null 2>&1 || true
        ufw allow 22/tcp             >/dev/null 2>&1 || true
        ufw allow "${PIER_PORT}"/tcp >/dev/null 2>&1 || true
        ufw allow 80/tcp             >/dev/null 2>&1 || true
        ufw allow 443/tcp            >/dev/null 2>&1 || true
        ufw allow 51820/udp          >/dev/null 2>&1 || true
        # Trust the WireGuard mesh subnet: intra-mesh traffic (cross-server DB
        # replication, remote service access) arrives on wg0 from 10.42.0.0/24
        # and is authenticated by WireGuard. Default subnet; adjust if changed.
        ufw allow from 10.42.0.0/24  >/dev/null 2>&1 || true
        # Cross-server DB clusters (mongo/redis/cassandra) are "every-node-to-
        # every": a node must reach ALL members — including itself — at
        # mesh-IP:published-port. A container hitting its OWN host's mesh IP
        # leaves with a docker-bridge source (172.x), so allow that source to
        # the cluster published-port band (10000-20000) only — NOT the panel
        # (8443) or ssh.
        ufw allow from 172.16.0.0/12 to any port 10000:20000 proto tcp >/dev/null 2>&1 || true
        if ufw --force enable >/dev/null 2>&1; then
            info "Firewall enabled"
        else
            warn "ufw enable failed — review the host firewall manually"
        fi
    else
        warn "ufw not installed — the host firewall is UNMANAGED."
        warn "Recommended: allow only SSH, ${PIER_PORT}/tcp (panel), 80,443/tcp (proxy), 51820/udp (mesh)."
    fi
fi

# Detect public IP for display
PUBLIC_IP=$(curl -s --max-time 5 https://api.ipify.org 2>/dev/null || hostname -I | awk '{print $1}')

# By now pier-core has either kept the token file (no admin yet) or removed
# it (admin already exists — fresh-token-after-consume, legacy-cleanup, or
# operator wiped users). The file is the source of truth.
SETUP_TOKEN=""
if [[ -f "$SETUP_TOKEN_FILE" ]]; then
    SETUP_TOKEN=$(cat "$SETUP_TOKEN_FILE")
fi

echo ""
echo -e "${GREEN}════════════════════════════════════════════════════════════${NC}"
echo -e "${GREEN}  Pier PaaS installed successfully!${NC}"
echo -e "${GREEN}════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  Dashboard:  ${YELLOW}https://${PUBLIC_IP}:${PIER_PORT}${NC}"
if [[ -n "$SETUP_TOKEN" ]]; then
    echo -e "  Setup:      ${YELLOW}https://${PUBLIC_IP}:${PIER_PORT}/setup?token=${SETUP_TOKEN}${NC}"
    echo -e "              ${GREEN}^ token is valid until the first admin is created${NC}"
fi
echo ""
echo -e "  Logs:       journalctl -u pier -f"
echo -e "  Status:     systemctl status pier"
echo -e "  Config:     ${PIER_ENV}"
echo -e "  Data:       ${PIER_DATA}"
echo ""
if [[ -n "$SETUP_TOKEN" ]]; then
    echo -e "  ${GREEN}Visit /setup to create your admin account.${NC}"
    echo -e "  ${YELLOW}Note:${NC} the panel uses a self-signed TLS cert on first run."
    echo -e "        Your browser will show a security warning — accept it to proceed."
fi
echo ""
echo -e "${YELLOW}════════════════════════════════════════════════════════════${NC}"
echo -e "${YELLOW}  Troubleshooting${NC}"
echo -e "${YELLOW}════════════════════════════════════════════════════════════${NC}"
echo ""
echo -e "  If /setup shows an error or pier doesn't respond:"
echo -e "    sudo journalctl -u pier -n 100 --no-pager"
echo ""
echo -e "  If Traefik (reverse proxy) failed to auto-start due to"
echo -e "  Docker Hub rate-limit (common on shared-IP VPS — log contains"
echo -e "  \"unauthenticated pull rate limit\"), pier will retry and fall"
echo -e "  back to public mirrors automatically. To force it right now:"
echo ""
echo -e "    docker pull mirror.gcr.io/library/traefik:v3.7.1"
echo -e "    docker tag mirror.gcr.io/library/traefik:v3.7.1 traefik:v3.7.1"
echo -e "    sudo systemctl restart pier"
echo ""
echo -e "  Pier detects the cached image locally and skips Docker Hub."
echo ""
