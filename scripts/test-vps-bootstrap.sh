#!/usr/bin/env bash
# Pier registry test harness — VPS bootstrap.
#
# Prepares an Ubuntu 24.04 host with every npm client the matrix exercises:
# Node 24 (NVM), pinned npm 7–11, yarn classic + yarn 4 (corepack), pnpm 9 + 10,
# bun. Then builds the pier-tests harness in release mode.
#
# Idempotent: re-running picks up where it stopped. Safe for a dedicated test
# VPS; do NOT run on a production Pier server — the rust build pulls ~2 GB of
# crates and pier-tests --keep can leave a stray child process.

set -euo pipefail

PIER_SRC="${PIER_SRC:-/opt/pier-src}"
CLIENTS_DIR="${CLIENTS_DIR:-/opt/pier-tests/clients}"
NODE_VERSION="${NODE_VERSION:-24}"

log() { printf '[bootstrap] %s\n' "$*"; }

# ── 1. NVM + Node 24 ──────────────────────────────────────────────────────
if [ ! -d "$HOME/.nvm" ]; then
    log "Installing nvm…"
    curl -fsSL https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash
fi
# shellcheck disable=SC1091
export NVM_DIR="$HOME/.nvm"
. "$NVM_DIR/nvm.sh"
nvm install "$NODE_VERSION"
nvm use "$NODE_VERSION"
corepack enable

# ── 2. Pinned npm versions (7–11) ─────────────────────────────────────────
mkdir -p "$CLIENTS_DIR"
for V in 7 8 9 10 11; do
    target="$CLIENTS_DIR/npm@$V"
    if [ ! -x "$target/bin/npm" ]; then
        log "Installing npm@$V → $target"
        npm install --silent --prefix "$target" "npm@$V" || true
    fi
done

# ── 3. yarn classic + yarn 4 ─────────────────────────────────────────────
log "corepack: yarn 1.22.22 + yarn 4.5.1"
corepack prepare yarn@1.22.22 --activate
ln -sf "$(command -v yarn)" "$CLIENTS_DIR/yarn@1"
corepack prepare yarn@4.5.1 --activate
ln -sf "$(command -v yarn)" "$CLIENTS_DIR/yarn@4"

# ── 4. pnpm 9 + 10 ────────────────────────────────────────────────────────
log "corepack: pnpm 9.15.0 + pnpm 10.0.0"
corepack prepare pnpm@9.15.0 --activate
ln -sf "$(command -v pnpm)" "$CLIENTS_DIR/pnpm@9"
corepack prepare pnpm@10.0.0 --activate
ln -sf "$(command -v pnpm)" "$CLIENTS_DIR/pnpm@10"

# ── 5. bun ───────────────────────────────────────────────────────────────
if [ ! -x "$HOME/.bun/bin/bun" ]; then
    log "Installing bun…"
    curl -fsSL https://bun.sh/install | bash
fi
ln -sf "$HOME/.bun/bin/bun" "$CLIENTS_DIR/bun@latest"

# ── 6. Rust + pier-tests build ───────────────────────────────────────────
if ! command -v cargo >/dev/null 2>&1; then
    log "Installing rustup + cargo…"
    curl -fsSL https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi
. "$HOME/.cargo/env"

if [ ! -d "$PIER_SRC/.git" ]; then
    log "Cloning Pier into $PIER_SRC (requires PIER_REPO env var or runs from existing checkout)"
    if [ -n "${PIER_REPO:-}" ]; then
        git clone "$PIER_REPO" "$PIER_SRC"
    else
        log "PIER_REPO not set and $PIER_SRC missing — skipping clone. Run from an existing checkout instead."
    fi
fi

if [ -d "$PIER_SRC/Pier" ]; then
    log "Building pier-tests…"
    (cd "$PIER_SRC/Pier" && cargo build --release -p pier-tests)
    ln -sf "$PIER_SRC/Pier/target/release/pier-tests" "$CLIENTS_DIR/pier-tests"
elif [ -d "$PIER_SRC/crates/pier-tests" ]; then
    log "Building pier-tests…"
    (cd "$PIER_SRC" && cargo build --release -p pier-tests)
    ln -sf "$PIER_SRC/target/release/pier-tests" "$CLIENTS_DIR/pier-tests"
fi

log "Done."
log ""
log "Run the matrix against an external Pier:"
log "  $CLIENTS_DIR/pier-tests --external-url https://YOUR-PIER --external-token pier_npm_… --report-md /tmp/report.md --report-junit /tmp/report.xml"
log ""
log "Run against a self-spawned Pier (requires /opt/pier-src/Pier/target/release/pier on PATH or --pier-bin):"
log "  $CLIENTS_DIR/pier-tests --report-md /tmp/report.md"
