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
#
# Also the engine of in-panel updates: /usr/local/sbin/pier-update runs this
# with PIER_SKIP_SYSTEM_PACKAGES=1 (skip steps 1-2 when Docker is present) and
# PIER_SKIP_FIREWALL=1 (honoured by install.sh).
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

# fetch URL DEST — download with retries that resume an interrupted transfer.
# GitHub is reachable but flaky from some networks (RU VPS): the ~36 MB binary
# often stalls mid-transfer, and a single curl then aborted the whole install
# or panel update. Each attempt continues from the bytes already on disk
# (-C -); under 1 KB/s for 30 s counts as a stall. The sha256 checks below
# still guard the result. Exit 22 = HTTP error (missing asset): no retry.
# Exit 33 = server refused the range: start that file over.
fetch() {
    local url=$1 dest=$2 attempt rc
    rm -f "$dest"
    for attempt in 1 2 3 4 5 6 7 8; do
        rc=0
        curl -fsSL -C - --connect-timeout 15 --speed-limit 1024 --speed-time 30 \
            "$url" -o "$dest" || rc=$?
        [[ $rc -eq 0 ]] && return 0
        [[ $rc -eq 22 ]] && return 1
        [[ $rc -eq 33 ]] && rm -f "$dest"
        if [[ $attempt -lt 8 ]]; then
            warn "Download of ${url##*/} interrupted (curl ${rc}), retrying ${attempt}/7..."
            sleep $((attempt * 2))
        fi
    done
    return 1
}

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

# Older installers always wrote Docker's ubuntu repo, even on Debian
# (issue #14). A leftover list for the wrong distro breaks every
# `apt-get update`, so drop it; get.docker.com rewrites it correctly.
heal_stale_docker_repo() {
    local os_ids list repo_distro
    os_ids=" $(. /etc/os-release && echo "${ID:-} ${ID_LIKE:-}") "
    for list in /etc/apt/sources.list.d/docker.list /etc/apt/sources.list.d/docker.sources; do
        [[ -f "$list" ]] || continue
        repo_distro=$(grep -oE 'download\.docker\.com/linux/[a-z]+' "$list" | head -n1 | cut -d/ -f3 || true)
        [[ -n "$repo_distro" ]] || continue
        if [[ "$os_ids" != *" ${repo_distro} "* ]]; then
            warn "Removing stale Docker repo ${list} (linux/${repo_distro} does not match this host)"
            rm -f "$list"
        fi
    done
}

# An update of an existing node skips apt entirely: the packages are already
# there, and an unrelated broken apt source must not be able to block it.
SKIP_SYSTEM_PACKAGES=false
if [[ "${PIER_SKIP_SYSTEM_PACKAGES:-0}" == "1" ]] && command -v docker &>/dev/null \
        && docker compose version &>/dev/null 2>&1 && command -v curl &>/dev/null; then
    SKIP_SYSTEM_PACKAGES=true
    info "Update mode: skipping base packages and Docker installation"
fi

if [[ "$SKIP_SYSTEM_PACKAGES" != true ]]; then
    step "Installing base packages (curl, ca-certificates, gnupg)..."
    export DEBIAN_FRONTEND=noninteractive
    heal_stale_docker_repo
    apt-get update -qq
    apt-get install -y -qq curl ca-certificates gnupg lsb-release >/dev/null
fi

# ── Step 2: Docker CE + Compose ──────────────────────────────────────────────

if command -v docker &>/dev/null && docker compose version &>/dev/null 2>&1; then
    info "Docker already installed: $(docker --version | grep -oP '\d+\.\d+\.\d+')"
else
    step "Installing Docker CE..."

    for pkg in docker.io docker-doc docker-compose podman-docker containerd runc; do
        apt-get remove -y -qq "$pkg" >/dev/null 2>&1 || true
    done

    # Docker's official script picks the right repo per distro
    # (Ubuntu, Debian, ...) — same path agent provisioning uses.
    curl -fsSL https://get.docker.com -o "${WORK_DIR}/get-docker.sh"
    sh "${WORK_DIR}/get-docker.sh" >/dev/null \
        || error "Docker install failed (get.docker.com). See output above."

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

if ! fetch "$BINARY_URL" "${WORK_DIR}/pier"; then
    error "Failed to download ${BINARY_URL}
Check that the release exists: https://github.com/${REPO}/releases"
fi

if ! fetch "$SHA_URL" "${WORK_DIR}/pier.sha256"; then
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
# pier-net-helper runs as root, so it is checked as strictly as the core: a
# published checksum that doesn't match aborts the install. A release without
# the .sha256 asset is still accepted (older releases), with a warning.
step "Downloading agent binaries (pier-agent, pier-net-helper)..."
for _agbin in pier-agent pier-net-helper; do
    _agurl="https://github.com/${REPO}/releases/download/${RELEASE_TAG}/${_agbin}-linux-amd64"
    if fetch "$_agurl" "${WORK_DIR}/${_agbin}"; then
        if fetch "${_agurl}.sha256" "${WORK_DIR}/${_agbin}.sha256"; then
            _exp=$(awk '{print $1}' "${WORK_DIR}/${_agbin}.sha256")
            _act=$(sha256sum "${WORK_DIR}/${_agbin}" | awk '{print $1}')
            if [[ -z "$_exp" || "$_exp" != "$_act" ]]; then
                error "Checksum mismatch for ${_agbin} — refusing to install
  Expected: ${_exp:-<empty>}
  Actual:   ${_act}"
            fi
        else
            warn "No published checksum for ${_agbin}; installing unverified"
        fi
        chmod +x "${WORK_DIR}/${_agbin}"
        info "Fetched ${_agbin}"
    else
        warn "Could not fetch ${_agbin} — agent enrollment will be unavailable until it is staged on the core"
    fi
done

# ── Step 5: Download install.sh + the systemd units ──────────────────────────

INSTALL_URL="https://raw.githubusercontent.com/${REPO}/${REF}/scripts/install.sh"

step "Downloading install.sh from ${REF}..."

if ! fetch "$INSTALL_URL" "${WORK_DIR}/install.sh"; then
    error "Failed to download ${INSTALL_URL}"
fi

chmod +x "${WORK_DIR}/install.sh"

# install.sh installs these verbatim when they sit next to it, and otherwise
# falls back to an inline copy. Fetching them keeps the units single-sourced
# from the repo for `curl | bash` installs too, so a unit fix (e.g. the
# Group=pier one from issue #9) reaches this path without an install.sh bump.
# Soft-fail: the inline fallback still covers a missing file.
for _unit in pier.service pier-net-helper.service \
        pier-updater.service pier-updater.path pier-update.sh; do
    _unit_url="https://raw.githubusercontent.com/${REPO}/${REF}/scripts/${_unit}"
    if ! fetch "$_unit_url" "${WORK_DIR}/${_unit}"; then
        rm -f "${WORK_DIR}/${_unit}"
        warn "Could not fetch ${_unit}; install.sh will use its inline copy."
    fi
done

# install.sh sources lib-swap.sh from its own dir for the 4 GiB swap floor;
# without it the swap step is skipped. Soft-fail like the units above.
if ! fetch "https://raw.githubusercontent.com/${REPO}/${REF}/scripts/lib-swap.sh" \
        "${WORK_DIR}/lib-swap.sh"; then
    rm -f "${WORK_DIR}/lib-swap.sh"
    warn "Could not fetch lib-swap.sh; install.sh will skip swap setup."
fi

# ── Step 6: Run install.sh ───────────────────────────────────────────────────

step "Running install.sh..."

bash "${WORK_DIR}/install.sh" --binary "${WORK_DIR}/pier" "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
