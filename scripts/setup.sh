#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Pier PaaS — One-Command Setup
# Installs all dependencies on a fresh Ubuntu server, then runs install.sh
#
# Usage (from extracted bundle):
#   sudo bash setup.sh
#   sudo bash setup.sh --port 9000
#
# This script:
#   1. Installs Docker CE + Docker Compose plugin
#   2. Installs git, curl
#   3. Calls install.sh --binary ./pier
# ============================================================================

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }
step()  { echo -e "${CYAN}[STEP]${NC}  $*"; }

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PIER_PORT=8443

# ── Parse arguments ──────────────────────────────────────────────────────────

EXTRA_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --port)
            PIER_PORT="$2"
            EXTRA_ARGS+=(--port "$2")
            shift 2
            ;;
        --help|-h)
            echo "Usage: sudo bash setup.sh [--port 8443]"
            echo ""
            echo "Fully automated Pier installation on a fresh Ubuntu/Debian/RHEL server."
            echo "Installs Docker, Docker Compose, git, then deploys Pier as a systemd service."
            exit 0
            ;;
        *)
            EXTRA_ARGS+=("$1")
            shift
            ;;
    esac
done

# ── Check root ──────────────────────────────────────────────────────────────

[[ $EUID -ne 0 ]] && error "This script must be run as root (sudo)"

# ── Check bundle files ──────────────────────────────────────────────────────

[[ ! -f "${SCRIPT_DIR}/pier" ]] && error "Binary 'pier' not found in ${SCRIPT_DIR}"
[[ ! -f "${SCRIPT_DIR}/install.sh" ]] && error "'install.sh' not found in ${SCRIPT_DIR}"

echo ""
echo -e "${CYAN}════════════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Pier PaaS — Automated Setup${NC}"
echo -e "${CYAN}════════════════════════════════════════════════════════════${NC}"
echo ""

# ── Step 1: Install system packages ─────────────────────────────────────────

step "Installing system packages..."

if command -v apt-get &>/dev/null; then
    export DEBIAN_FRONTEND=noninteractive
    apt-get update -qq
    apt-get install -y -qq curl git ca-certificates gnupg lsb-release >/dev/null 2>&1
elif command -v dnf &>/dev/null; then
    dnf install -y -q curl git ca-certificates gnupg >/dev/null 2>&1
elif command -v yum &>/dev/null; then
    yum install -y -q curl git ca-certificates gnupg >/dev/null 2>&1
else
    error "Unsupported OS. This script supports Ubuntu/Debian (apt), Fedora (dnf), CentOS/RHEL (yum)."
fi

info "System packages OK"

# ── Step 2: Install Docker CE + Compose ──────────────────────────────────────

if command -v docker &>/dev/null && docker compose version &>/dev/null; then
    info "Docker already installed: $(docker --version | grep -oP '\d+\.\d+\.\d+')"
else
    step "Installing Docker CE..."

    if command -v apt-get &>/dev/null; then
        # Remove old packages
        for pkg in docker.io docker-doc docker-compose podman-docker containerd runc; do
            apt-get remove -y -qq "$pkg" 2>/dev/null || true
        done

        # Add Docker GPG key + repo
        install -m 0755 -d /etc/apt/keyrings
        curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
        chmod a+r /etc/apt/keyrings/docker.asc

        echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] \
https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" \
            > /etc/apt/sources.list.d/docker.list

        apt-get update -qq
        apt-get install -y -qq docker-ce docker-ce-cli containerd.io docker-compose-plugin >/dev/null 2>&1

    elif command -v dnf &>/dev/null || command -v yum &>/dev/null; then
        PKG=$(command -v dnf &>/dev/null && echo "dnf" || echo "yum")
        $PKG install -y -q yum-utils >/dev/null 2>&1 || true
        yum-config-manager --add-repo https://download.docker.com/linux/centos/docker-ce.repo 2>/dev/null
        $PKG install -y -q docker-ce docker-ce-cli containerd.io docker-compose-plugin >/dev/null 2>&1
    fi

    systemctl enable docker
    systemctl start docker

    info "Docker installed: $(docker --version | grep -oP '\d+\.\d+\.\d+')"
fi

# Verify Docker is running
if ! docker info &>/dev/null; then
    error "Docker daemon failed to start. Check: systemctl status docker"
fi

# ── Step 3: Run Pier install.sh ──────────────────────────────────────────────

step "Installing Pier..."

bash "${SCRIPT_DIR}/install.sh" --binary "${SCRIPT_DIR}/pier" "${EXTRA_ARGS[@]+"${EXTRA_ARGS[@]}"}"
