#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Pier PaaS — Bootstrap Installer
# Downloads the latest pre-built binary from GitHub Releases and installs Pier
# as a systemd service on a fresh Ubuntu/Debian server.
#
# Usage:
#   curl -fsSL https://pier.team/install | sudo bash
#   curl -fsSL https://pier.team/install | sudo bash -s -- --port 9000
#
# Or directly:
#   curl -fsSL https://raw.githubusercontent.com/joveptesg/pier/main/scripts/bootstrap.sh | sudo bash
#
# This script:
#   1. Installs Docker CE + Compose plugin (if missing)
#   2. Downloads pier-linux-amd64 from GitHub Releases (tag: latest)
#   3. Verifies the binary against its published sha256
#   4. Downloads install.sh from the repo
#   5. Runs install.sh --binary <downloaded-pier>
# ============================================================================

REPO="joveptesg/pier"
REF="${PIER_REF:-main}"
RELEASE_TAG="${PIER_RELEASE_TAG:-latest}"
BINARY_NAME="pier-linux-amd64"

PIER_PORT=8443

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }
step()  { echo -e "${CYAN}[STEP]${NC}  $*"; }

# ── Parse arguments ──────────────────────────────────────────────────────────

EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)
            PIER_PORT="$2"
            EXTRA_ARGS+=(--port "$2")
            shift 2
            ;;
        --ref)
            REF="$2"
            shift 2
            ;;
        --release-tag)
            RELEASE_TAG="$2"
            shift 2
            ;;
        --help|-h)
            cat <<EOF
Usage: sudo bash bootstrap.sh [options]

Options:
  --port PORT            HTTP listen port for Pier dashboard (default: 8443)
  --ref REF              Git ref for install.sh (branch/tag/commit, default: main)
  --release-tag TAG      GitHub release tag to download binary from (default: latest)
  --help, -h             Show this help

Environment variables:
  PIER_REF               Same as --ref
  PIER_RELEASE_TAG       Same as --release-tag
EOF
            exit 0
            ;;
        *)
            EXTRA_ARGS+=("$1")
            shift
            ;;
    esac
done

# ── Sanity checks ────────────────────────────────────────────────────────────

[[ $EUID -ne 0 ]] && error "This script must be run as root (sudo)"

if ! command -v apt-get &>/dev/null; then
    error "This bootstrap supports apt-based systems (Ubuntu/Debian) only.
For RHEL/Fedora/Alpine, follow the manual steps in INSTALL.md:
  https://github.com/${REPO}/blob/${REF}/INSTALL.md"
fi

ARCH=$(uname -m)
if [[ "$ARCH" != "x86_64" ]]; then
    error "Only x86_64 is published in releases right now (got: $ARCH).
Build from source via INSTALL.md for other architectures."
fi

# ── Workspace ────────────────────────────────────────────────────────────────

WORK_DIR=$(mktemp -d -t pier-bootstrap-XXXXXX)
trap 'rm -rf "$WORK_DIR"' EXIT

echo ""
echo -e "${CYAN}════════════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Pier PaaS — Bootstrap Installer${NC}"
echo -e "${CYAN}  Repo: ${REPO}  Ref: ${REF}  Release: ${RELEASE_TAG}${NC}"
echo -e "${CYAN}════════════════════════════════════════════════════════════${NC}"
echo ""

# ── Step 1: Base system packages ─────────────────────────────────────────────

step "Installing base packages (curl, ca-certificates, gnupg)..."
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq curl ca-certificates gnupg lsb-release >/dev/null

# ── Step 2: Docker CE + Compose ──────────────────────────────────────────────

if command -v docker &>/dev/null && docker compose version &>/dev/null 2>&1; then
    info "Docker already installed: $(docker --version | grep -oP '\d+\.\d+\.\d+')"
else
    step "Installing Docker CE..."

    for pkg in docker.io docker-doc docker-compose podman-docker containerd runc; do
        apt-get remove -y -qq "$pkg" >/dev/null 2>&1 || true
    done

    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
    chmod a+r /etc/apt/keyrings/docker.asc

    echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
        > /etc/apt/sources.list.d/docker.list

    apt-get update -qq
    apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-compose-plugin >/dev/null

    systemctl enable docker >/dev/null 2>&1
    systemctl start docker

    info "Docker installed: $(docker --version | grep -oP '\d+\.\d+\.\d+')"
fi

if ! docker info &>/dev/null; then
    error "Docker daemon failed to start. Check: systemctl status docker"
fi

# ── Step 3: Download binary + checksum ──────────────────────────────────────

BINARY_URL="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${BINARY_NAME}"
SHA_URL="${BINARY_URL}.sha256"

step "Downloading Pier binary from ${RELEASE_TAG} release..."

if ! curl -fsSL "$BINARY_URL" -o "${WORK_DIR}/pier"; then
    error "Failed to download ${BINARY_URL}
Check that the release exists: https://github.com/${REPO}/releases"
fi

if ! curl -fsSL "$SHA_URL" -o "${WORK_DIR}/pier.sha256"; then
    error "Failed to download checksum from ${SHA_URL}"
fi

# ── Step 4: Verify checksum ──────────────────────────────────────────────────

step "Verifying binary checksum..."

EXPECTED=$(awk '{print $1}' "${WORK_DIR}/pier.sha256")
ACTUAL=$(sha256sum "${WORK_DIR}/pier" | awk '{print $1}')

if [[ -z "$EXPECTED" ]]; then
    error "Empty checksum from ${SHA_URL}"
fi

if [[ "$EXPECTED" != "$ACTUAL" ]]; then
    error "Checksum mismatch — refusing to install
  Expected: ${EXPECTED}
  Actual:   ${ACTUAL}"
fi

info "Checksum OK (sha256: ${ACTUAL:0:16}...)"
chmod +x "${WORK_DIR}/pier"

# ── Step 4b: Agent binaries (staged next to the core) ────────────────────────
# The core SERVES pier-agent + pier-net-helper to enrolling agents from its own
# bin dir (GET /api/v1/servers/download/{name}), so they must sit next to the
# core binary for install.sh to stage them into /opt/pier/bin. Soft-fail: a
# single-node core works fine without them; only agent enrollment needs them.
step "Downloading agent binaries (pier-agent, pier-net-helper)..."
for _agbin in pier-agent pier-net-helper; do
    _agurl="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${_agbin}-linux-amd64"
    if curl -fsSL "$_agurl" -o "${WORK_DIR}/${_agbin}"; then
        chmod +x "${WORK_DIR}/${_agbin}"
        info "Fetched ${_agbin}"
    else
        warn "Could not fetch ${_agbin} — agent enrollment will be unavailable until it is staged on the core"
    fi
done

# ── Step 5: Download install.sh ──────────────────────────────────────────────

INSTALL_URL="https://raw.githubusercontent.com/${REPO}/${REF}/scripts/install.sh"

step "Downloading install.sh from ${REF}..."

if ! curl -fsSL "$INSTALL_URL" -o "${WORK_DIR}/install.sh"; then
    error "Failed to download ${INSTALL_URL}"
fi

chmod +x "${WORK_DIR}/install.sh"

# ── Step 6: Run install.sh ───────────────────────────────────────────────────

step "Running install.sh..."

bash "${WORK_DIR}/install.sh" --binary "${WORK_DIR}/pier" "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
