#!/usr/bin/env bash
# ============================================================================
# Pier self-updater — installed as /usr/local/sbin/pier-update.
#
# Run as root by pier-updater.service, which pier-updater.path starts when the
# panel drops /opt/pier/data/update-request. It re-runs the official
# `curl | bash` path (bootstrap.sh → install.sh), so one click refreshes
# EVERYTHING the installer owns: the pier binary, pier-net-helper and
# pier-agent, every systemd unit (this updater's own included) — the parts the
# sandboxed pier service can never touch itself.
#
# Trust boundary: the panel can only ask for an update. The request file's
# contents are never read; what gets installed is decided here — the latest
# release, sha256-verified by bootstrap.sh. Status and log live in a
# root-owned directory so the pier user can read them but cannot plant a
# symlink that would make this root process write somewhere else.
#
# Deliberately skipped compared to a manual run: apt/Docker installation (the
# node already has them, and a broken third-party apt source must not block an
# update) and firewall changes (an operator who turned ufw off keeps it off).
# ============================================================================

set -uo pipefail

REQUEST=/opt/pier/data/update-request
STATE_DIR=/var/lib/pier-updater
STATUS="${STATE_DIR}/status.json"
LOG="${STATE_DIR}/last.log"
REPO="${PIER_UPDATE_REPO:-joveptesg/pier}"
REF="${PIER_UPDATE_REF:-main}"

# Consume the request first: the path unit uses PathExists=, so a request left
# behind by a failed run would trigger it again in a loop.
rm -f "$REQUEST"

mkdir -p "$STATE_DIR"
chmod 755 "$STATE_DIR"
STARTED=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Messages are fixed strings (no quotes/backslashes), so plain printf is valid
# JSON.
write_status() {
    printf '{"state":"%s","message":"%s","started_at":"%s","updated_at":"%s"}\n' \
        "$1" "$2" "$STARTED" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" >"${STATUS}.tmp"
    chmod 644 "${STATUS}.tmp"
    mv -f "${STATUS}.tmp" "$STATUS"
}

: >"$LOG"
# The log can include the one-time /setup URL on a node without an admin;
# keep it to root and the pier group (the panel), not world-readable.
chown root:pier "$LOG" 2>/dev/null || true
chmod 640 "$LOG"

write_status running "Downloading the installer"
echo "[pier-update] ${STARTED} — updating from ${REPO}@${REF}" >>"$LOG"

WORK=$(mktemp -d -t pier-update-XXXXXX)
trap 'rm -rf "$WORK"' EXIT

if ! curl -fsSL --retry 3 --max-time 60 \
        "https://raw.githubusercontent.com/${REPO}/${REF}/scripts/bootstrap.sh" \
        -o "${WORK}/bootstrap.sh" >>"$LOG" 2>&1; then
    echo "[pier-update] could not download bootstrap.sh" >>"$LOG"
    write_status failed "Could not download the installer"
    exit 1
fi

write_status running "Installing the update"
PIER_SKIP_SYSTEM_PACKAGES=1 PIER_SKIP_FIREWALL=1 PIER_REF="$REF" \
    bash "${WORK}/bootstrap.sh" >>"$LOG" 2>&1
rc=$?

if [[ $rc -eq 0 ]]; then
    echo "[pier-update] done" >>"$LOG"
    write_status success "Update installed"
else
    echo "[pier-update] installer exited with code ${rc}" >>"$LOG"
    write_status failed "The installer failed, see the log"
fi
exit $rc
