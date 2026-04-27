#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Pier PaaS — Adaptive Build From Source
# Detects available RAM, sets up swap if needed, picks a cargo profile and
# job count that fits the machine, builds pier, then chains install.sh.
#
# Usage:
#   sudo bash scripts/build-from-source.sh
#   sudo bash scripts/build-from-source.sh --no-swap --profile release-lowmem
#   sudo bash scripts/build-from-source.sh --jobs 2 --no-install
#
# Strategy by effective_ram = MemTotal + SwapTotal:
#   >= 6 GB    → profile=release,         jobs=nproc
#   3-6 GB     → profile=release,         jobs=ram_gb/2
#   <  3 GB    → profile=release-lowmem,  jobs=1, ensure swap up to 4 GB total
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
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
SWAPFILE="/swapfile"

# ── Parse arguments ──────────────────────────────────────────────────────────

NO_SWAP=false
NO_INSTALL=false
ASSUME_YES=false
FORCE_PROFILE=""
FORCE_JOBS=""
PIER_PORT=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-swap)     NO_SWAP=true;     shift ;;
        --no-install)  NO_INSTALL=true;  shift ;;
        -y|--yes)      ASSUME_YES=true;  shift ;;
        --profile)     FORCE_PROFILE="$2"; shift 2 ;;
        --jobs)        FORCE_JOBS="$2";    shift 2 ;;
        --port)        PIER_PORT="$2";     shift 2 ;;
        --help|-h)
            cat <<EOF
Usage: sudo bash scripts/build-from-source.sh [options]

Builds pier from source with a strategy adapted to the machine's RAM,
then runs install.sh --binary <built-binary>.

Options:
  --no-swap         Do not create swap even if RAM is low (forces release-lowmem)
  --no-install      Build only, skip install.sh
  -y, --yes         Skip the confirmation prompt before creating swap
  --profile NAME    Force cargo profile (release | release-lowmem)
  --jobs N          Force cargo --jobs (CARGO_BUILD_JOBS)
  --port PORT       Forwarded to install.sh (default: 8443)
  -h, --help        Show this help
EOF
            exit 0
            ;;
        *) error "Unknown option: $1. Use --help for usage." ;;
    esac
done

# ── Check root ───────────────────────────────────────────────────────────────

[[ $EUID -ne 0 ]] && error "This script must be run as root (sudo). It needs root for swap setup and install.sh."

# Pick a non-root user for cargo. SUDO_USER is set when invoked via sudo.
BUILD_USER="${SUDO_USER:-root}"
if [[ "$BUILD_USER" == "root" ]]; then
    warn "No SUDO_USER detected — cargo will run as root. Prefer 'sudo bash scripts/build-from-source.sh' from a normal user."
fi

# ── Detect resources ─────────────────────────────────────────────────────────

step "Detecting machine resources..."

MEM_TOTAL_KB=$(awk '/^MemTotal:/ {print $2}' /proc/meminfo)
SWAP_TOTAL_KB=$(awk '/^SwapTotal:/ {print $2}' /proc/meminfo)
MEM_TOTAL_MB=$(( MEM_TOTAL_KB / 1024 ))
SWAP_TOTAL_MB=$(( SWAP_TOTAL_KB / 1024 ))
EFFECTIVE_MB=$(( MEM_TOTAL_MB + SWAP_TOTAL_MB ))
NPROC=$(nproc)

info "RAM: ${MEM_TOTAL_MB} MiB, Swap: ${SWAP_TOTAL_MB} MiB, CPUs: ${NPROC}"

# ── Pick strategy ────────────────────────────────────────────────────────────

SWAP_ADD_MB=0
PROFILE="release"
JOBS="$NPROC"

if (( EFFECTIVE_MB >= 6144 )); then
    PROFILE="release"
    JOBS="$NPROC"
elif (( EFFECTIVE_MB >= 3072 )); then
    PROFILE="release"
    # ram_gb/2, at least 1
    JOBS=$(( MEM_TOTAL_MB / 2048 ))
    (( JOBS < 1 )) && JOBS=1
    (( JOBS > NPROC )) && JOBS="$NPROC"
else
    PROFILE="release-lowmem"
    JOBS=1
    if [[ "$NO_SWAP" == "false" ]]; then
        # Bring effective memory up to 4 GiB.
        TARGET_MB=4096
        ADD=$(( TARGET_MB - EFFECTIVE_MB ))
        # round up to whole GiB, min 1 GiB if we add anything at all
        SWAP_ADD_MB=$(( ((ADD + 1023) / 1024) * 1024 ))
        (( SWAP_ADD_MB < 1024 )) && SWAP_ADD_MB=1024
    fi
fi

# Honor forces last so user overrides win.
[[ -n "$FORCE_PROFILE" ]] && PROFILE="$FORCE_PROFILE"
[[ -n "$FORCE_JOBS" ]]    && JOBS="$FORCE_JOBS"

# Disk-space sanity for swap
if (( SWAP_ADD_MB > 0 )); then
    DISK_FREE_MB=$(df -BM --output=avail / | tail -n1 | tr -dc '0-9')
    NEEDED_MB=$(( SWAP_ADD_MB + 2048 ))
    if (( DISK_FREE_MB < NEEDED_MB )); then
        warn "Only ${DISK_FREE_MB} MiB free on /, need ${NEEDED_MB} MiB for swap + 2 GiB headroom."
        warn "Falling back to release-lowmem without swap. Build may run very slow or OOM."
        SWAP_ADD_MB=0
    fi
fi

# ── Print plan ───────────────────────────────────────────────────────────────

echo ""
echo -e "${CYAN}════════════════════════════════════════════════════════════${NC}"
echo -e "${CYAN}  Build plan${NC}"
echo -e "${CYAN}════════════════════════════════════════════════════════════${NC}"
echo "  Profile:    ${PROFILE}"
echo "  Jobs:       ${JOBS}"
if (( SWAP_ADD_MB > 0 )); then
    echo "  Swap:       create ${SWAP_ADD_MB} MiB at ${SWAPFILE}, persist in /etc/fstab"
else
    echo "  Swap:       no changes"
fi
echo "  Build user: ${BUILD_USER}"
echo "  Repo:       ${REPO_DIR}"
echo ""

if (( SWAP_ADD_MB > 0 )) && [[ "$ASSUME_YES" == "false" ]]; then
    read -r -p "Proceed with this plan? [Y/n] " ans
    case "${ans:-Y}" in
        Y|y|yes|YES) ;;
        *) error "Aborted by user." ;;
    esac
fi

# ── Ensure swap ──────────────────────────────────────────────────────────────

if (( SWAP_ADD_MB > 0 )); then
    step "Setting up ${SWAP_ADD_MB} MiB swap at ${SWAPFILE}..."

    if swapon --show=NAME --noheadings | grep -qx "$SWAPFILE"; then
        info "${SWAPFILE} already active — skipping creation."
    else
        if [[ -e "$SWAPFILE" ]]; then
            warn "${SWAPFILE} exists but is not active. Reusing it as-is."
        else
            if command -v fallocate &>/dev/null; then
                fallocate -l "${SWAP_ADD_MB}M" "$SWAPFILE"
            else
                dd if=/dev/zero of="$SWAPFILE" bs=1M count="$SWAP_ADD_MB" status=progress
            fi
            chmod 600 "$SWAPFILE"
            mkswap "$SWAPFILE" >/dev/null
        fi
        swapon "$SWAPFILE"
        info "Swap activated."
    fi

    if ! grep -qE "^${SWAPFILE}\s+" /etc/fstab; then
        echo "${SWAPFILE} none swap sw 0 0" >> /etc/fstab
        info "Added ${SWAPFILE} to /etc/fstab."
    else
        info "${SWAPFILE} already in /etc/fstab — skipping."
    fi
fi

# ── Ensure rust toolchain (as build user) ───────────────────────────────────

step "Checking Rust toolchain (user: ${BUILD_USER})..."

run_as_build_user() {
    if [[ "$BUILD_USER" == "root" ]]; then
        bash -c "$1"
    else
        sudo -u "$BUILD_USER" -H bash -c "$1"
    fi
}

if ! run_as_build_user "command -v rustc >/dev/null && command -v cargo >/dev/null && [ -f \"\$HOME/.cargo/env\" ] || command -v rustc >/dev/null"; then
    warn "Rust not found for ${BUILD_USER}. Installing rustup..."
    run_as_build_user "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable"
fi

RUSTC_VER=$(run_as_build_user "source \$HOME/.cargo/env 2>/dev/null || true; rustc --version")
info "Rust: ${RUSTC_VER}"

# ── Build ────────────────────────────────────────────────────────────────────

step "Building pier (profile=${PROFILE}, jobs=${JOBS})..."

CARGO_CMD="source \$HOME/.cargo/env 2>/dev/null || true; cd '${REPO_DIR}' && CARGO_BUILD_JOBS=${JOBS} cargo build --profile ${PROFILE} --package pier-core"
run_as_build_user "$CARGO_CMD"

if [[ "$PROFILE" == "release" ]]; then
    BINARY_PATH="${REPO_DIR}/target/release/pier"
else
    BINARY_PATH="${REPO_DIR}/target/${PROFILE}/pier"
fi

[[ ! -f "$BINARY_PATH" ]] && error "Build finished but binary not found at ${BINARY_PATH}"
info "Built: ${BINARY_PATH} ($(du -h "$BINARY_PATH" | cut -f1))"

# ── Chain install.sh ─────────────────────────────────────────────────────────

if [[ "$NO_INSTALL" == "true" ]]; then
    info "Skipping install.sh (--no-install). Run it manually:"
    echo "  sudo bash ${SCRIPT_DIR}/install.sh --binary ${BINARY_PATH}"
    exit 0
fi

step "Running install.sh..."
INSTALL_ARGS=(--binary "$BINARY_PATH")
[[ -n "$PIER_PORT" ]] && INSTALL_ARGS+=(--port "$PIER_PORT")

bash "${SCRIPT_DIR}/install.sh" "${INSTALL_ARGS[@]}"
