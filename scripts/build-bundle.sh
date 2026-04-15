#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
# Pier PaaS — Build Bundle Script
# Compiles Pier for Linux x86_64 and packages into a distributable archive
#
# Usage:
#   bash scripts/build-bundle.sh                    # build only
#   bash scripts/build-bundle.sh --upload S3_URL    # build + upload to S3
#
# Requirements:
#   - Docker (for cross-compilation)
#   - OR: cargo + cross (cargo install cross)
#   - aws cli / curl (for --upload)
# ============================================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PIER_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUNDLE_DIR="${PIER_ROOT}/dist/pier-bundle"
BUNDLE_NAME="pier-bundle.tar.gz"
TARGET="x86_64-unknown-linux-gnu"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
step()  { echo -e "${CYAN}[STEP]${NC}  $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; exit 1; }

# ── Parse arguments ──────────────────────────────────────────────────────────

UPLOAD_URL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --upload)
            UPLOAD_URL="$2"
            shift 2
            ;;
        --help|-h)
            echo "Usage: bash scripts/build-bundle.sh [--upload S3_URL]"
            echo ""
            echo "Options:"
            echo "  --upload URL   Upload bundle to S3-compatible storage after build"
            echo "                 Examples:"
            echo "                   --upload s3://my-bucket/pier/"
            echo "                   --upload https://storage.bunnycdn.com/pier/"
            exit 0
            ;;
        *)
            error "Unknown option: $1"
            ;;
    esac
done

# ── Step 1: Compile Pier for Linux ──────────────────────────────────────────

step "Compiling Pier for ${TARGET}..."

cd "$PIER_ROOT"

# Try cross first (best for cross-compilation from macOS/Windows)
if command -v cross &>/dev/null; then
    info "Using 'cross' for cross-compilation"
    cross build --release --package pier-core --target "$TARGET"
    BINARY="${PIER_ROOT}/target/${TARGET}/release/pier-core"
# If already on Linux, use cargo directly
elif [[ "$(uname -s)" == "Linux" ]] && [[ "$(uname -m)" == "x86_64" ]]; then
    info "Building natively on Linux x86_64"
    cargo build --release --package pier-core
    BINARY="${PIER_ROOT}/target/release/pier-core"
# Fallback: build inside Docker
else
    info "Using Docker for cross-compilation"
    docker run --rm \
        -v "${PIER_ROOT}:/app" \
        -w /app \
        rust:1.93-slim \
        bash -c "apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev >/dev/null 2>&1 && cargo build --release --package pier-core"
    BINARY="${PIER_ROOT}/target/release/pier-core"
fi

[[ ! -f "$BINARY" ]] && error "Build failed: binary not found at $BINARY"

BINARY_SIZE=$(du -h "$BINARY" | cut -f1)
info "Binary compiled: ${BINARY_SIZE}"

# ── Step 2: Package bundle ──────────────────────────────────────────────────

step "Packaging bundle..."

rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR"

# Copy binary (rename to 'pier')
cp "$BINARY" "${BUNDLE_DIR}/pier"
chmod 755 "${BUNDLE_DIR}/pier"

# Copy scripts + service unit
cp "${SCRIPT_DIR}/setup.sh" "${BUNDLE_DIR}/setup.sh"
cp "${SCRIPT_DIR}/install.sh" "${BUNDLE_DIR}/install.sh"
cp "${SCRIPT_DIR}/pier.service" "${BUNDLE_DIR}/pier.service"

# Create bundle archive
cd "${PIER_ROOT}/dist"
tar -czf "$BUNDLE_NAME" -C pier-bundle .

BUNDLE_SIZE=$(du -h "$BUNDLE_NAME" | cut -f1)
info "Bundle created: dist/${BUNDLE_NAME} (${BUNDLE_SIZE})"

# ── Step 3: Upload (optional) ──────────────────────────────────────────────

if [[ -n "$UPLOAD_URL" ]]; then
    step "Uploading to ${UPLOAD_URL}..."

    BUNDLE_PATH="${PIER_ROOT}/dist/${BUNDLE_NAME}"

    # S3 via aws cli
    if [[ "$UPLOAD_URL" == s3://* ]]; then
        if ! command -v aws &>/dev/null; then
            error "aws cli not installed. Install: pip install awscli"
        fi
        aws s3 cp "$BUNDLE_PATH" "${UPLOAD_URL}${BUNDLE_NAME}"
        info "Uploaded to ${UPLOAD_URL}${BUNDLE_NAME}"

    # HTTP PUT (Bunny CDN, generic S3-compatible)
    elif [[ "$UPLOAD_URL" == https://* ]] || [[ "$UPLOAD_URL" == http://* ]]; then
        if [[ -n "${STORAGE_API_KEY:-}" ]]; then
            curl -sSf -X PUT \
                -H "AccessKey: ${STORAGE_API_KEY}" \
                --data-binary @"$BUNDLE_PATH" \
                "${UPLOAD_URL}${BUNDLE_NAME}"
        else
            curl -sSf -X PUT \
                --data-binary @"$BUNDLE_PATH" \
                "${UPLOAD_URL}${BUNDLE_NAME}"
        fi
        info "Uploaded to ${UPLOAD_URL}${BUNDLE_NAME}"

    else
        warn "Unknown upload protocol. Skipping upload."
        warn "Bundle is at: dist/${BUNDLE_NAME}"
    fi
else
    echo ""
    info "Bundle ready: dist/${BUNDLE_NAME}"
    echo ""
    echo "  Upload manually:"
    echo "    aws s3 cp dist/${BUNDLE_NAME} s3://your-bucket/pier/"
    echo "    scp dist/${BUNDLE_NAME} user@server:/root/"
    echo ""
    echo "  Install on server:"
    echo "    tar xzf ${BUNDLE_NAME} && sudo bash setup.sh"
fi

# ── Cleanup ─────────────────────────────────────────────────────────────────

rm -rf "$BUNDLE_DIR"

echo ""
echo -e "${GREEN}Done.${NC}"
