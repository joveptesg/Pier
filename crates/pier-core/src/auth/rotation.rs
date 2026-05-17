//! Scheduled long-term `agent_token` rotation.
//!
//! Background task that wakes up once an hour, finds every agent-kind
//! server whose `token_rotated_at` is older than the configured rotation
//! interval (default 7 days), and rotates it via
//! [`api::servers::rotate_token_internal`].
//!
//! Settings the scheduler honours (read each tick — operator can change
//! them without restarting the binary):
//!
//! * `rotation.enabled` (`"true"` / `"false"`, default true) — kill
//!   switch for environments where the operator wants to manage rotation
//!   manually.
//! * `rotation.interval_days` (default `"7"`) — wall-clock age after
//!   which a token is "due". 0 means "rotate on every tick" — useful
//!   for soak-testing the rotation path.
//!
//! Failure handling: each rotation is independent. An agent that refuses
//! its rotate call (offline, refused write, etc.) is logged at WARN
//! level and skipped; the next tick will retry. We deliberately don't
//! mark the agent unhealthy here because the legacy `agent_token`
//! column still lets it heartbeat — degraded auth is preferable to
//! a hard outage triggered by the rotator itself.

use std::time::Duration;

use tokio::time::interval;

use crate::api::servers::rotate_token_internal;
use crate::state::SharedState;

const TICK_INTERVAL_SECS: u64 = 3600; // re-evaluate hourly
const INITIAL_DELAY_SECS: u64 = 300; // ~5 min after boot
const DEFAULT_INTERVAL_DAYS: i64 = 7;

pub fn start_scheduler(state: SharedState) {
    tokio::spawn(async move {
        // Let migrations, federation sync, and the heartbeat task settle
        // before we start touching tokens. A rotation triggered before
        // a fresh agent has finished its first handshake would be
        // pointless.
        tokio::time::sleep(Duration::from_secs(INITIAL_DELAY_SECS)).await;

        let mut tick = interval(Duration::from_secs(TICK_INTERVAL_SECS));
        loop {
            tick.tick().await;
            if !rotation_enabled(&state) {
                continue;
            }
            let interval_days = rotation_interval_days(&state);
            let cutoff = chrono::Utc::now().timestamp() - interval_days * 86400;
            let due = match collect_due(&state, cutoff) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("rotation: collect_due failed: {e}");
                    continue;
                }
            };
            for (id, name) in due {
                match rotate_token_internal(&state, &id).await {
                    Ok(outcome) => tracing::info!(
                        "rotation: agent {name} ({id}) rotated to prefix {} at {}",
                        outcome.prefix,
                        outcome.rotated_at
                    ),
                    Err(e) => tracing::warn!(
                        "rotation: agent {name} ({id}) rotation failed: {e}; will retry next tick"
                    ),
                }
            }

            // Federation tokens for paired peers follow the same
            // cadence — the cliff between old and new is shorter for
            // them (peer's UPDATE happens in-band with the rotate
            // call) but the pressure for rotation is the same.
            let due_fed = match collect_due_federation(&state, cutoff) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("rotation: collect_due_federation failed: {e}");
                    continue;
                }
            };
            for (id, name) in due_fed {
                match rotate_federation_token_internal(&state, &id).await {
                    Ok(prefix) => tracing::info!(
                        "rotation: peer {name} ({id}) federation token rotated to {prefix}"
                    ),
                    Err(e) => tracing::warn!(
                        "rotation: peer {name} ({id}) federation rotation failed: {e}; will retry next tick"
                    ),
                }
            }
        }
    });
}

/// Identical to `collect_due` but for `kind='peer'` rows with a paired
/// `federation_token`. NULL `federation_token_rotated_at` is treated as
/// "rotate-on-first-tick" — same semantics as the agent side.
fn collect_due_federation(
    state: &SharedState,
    cutoff: i64,
) -> anyhow::Result<Vec<(String, String)>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name FROM servers \
         WHERE kind = 'peer' \
           AND is_local = 0 \
           AND federation_token IS NOT NULL \
           AND federation_token <> '' \
           AND (federation_token_rotated_at IS NULL OR federation_token_rotated_at < ?1) \
         ORDER BY federation_token_rotated_at NULLS FIRST, name",
    )?;
    let rows = stmt
        .query_map([cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

/// Drive one peer's federation token rotation. Steps:
///   1. Resolve a `WritePeer` from current `servers.federation_token`.
///   2. Call peer's `/api/v1/agent/rotate-token`; peer mints + persists
///      a new hash for the same row id, returns plaintext.
///   3. Persist plaintext to `servers.federation_token` and stamp
///      `federation_token_rotated_at`.
///
/// If step 3 fails the operator is left with peer hashed-and-updated
/// but primary still holding the old token — the next federation call
/// will 401. We deliberately log+continue rather than try to roll back
/// the peer (peer's rotation is intentionally one-way for simplicity);
/// the operator can re-pair manually from /servers/<id>.
async fn rotate_federation_token_internal(
    state: &SharedState,
    server_id: &str,
) -> anyhow::Result<String> {
    let peer = crate::federation::write_client::lookup_write_peer(state, server_id)?
        .ok_or_else(|| anyhow::anyhow!("peer {server_id} not paired for federation"))?;
    let new_plaintext = crate::federation::write_client::rotate_token(&peer).await?;
    let now = chrono::Utc::now().timestamp();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE servers \
             SET federation_token = ?1, federation_token_rotated_at = ?2, \
                 updated_at = datetime('now') \
             WHERE id = ?3",
            rusqlite::params![new_plaintext, now, server_id],
        )?;
    }
    // Return the visible prefix only — we never log the full plaintext.
    Ok(new_plaintext.chars().take(16).collect())
}

fn rotation_enabled(state: &SharedState) -> bool {
    let Ok(db) = state.db.lock() else { return true };
    let v: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'rotation.enabled'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "true".to_string());
    v != "false"
}

fn rotation_interval_days(state: &SharedState) -> i64 {
    let Ok(db) = state.db.lock() else {
        return DEFAULT_INTERVAL_DAYS;
    };
    let v: String = match db.query_row(
        "SELECT value FROM settings WHERE key = 'rotation.interval_days'",
        [],
        |row| row.get(0),
    ) {
        Ok(s) => s,
        Err(_) => return DEFAULT_INTERVAL_DAYS,
    };
    v.parse().unwrap_or(DEFAULT_INTERVAL_DAYS)
}

/// Return `(id, name)` for every agent-kind row whose token is older
/// than `cutoff` (unix seconds). `token_rotated_at IS NULL` is treated
/// as "needs rotation" — these are agents still on their original
/// handshake-issued token. We exclude rows with no `agent_token` yet
/// (bootstrap pending) since there's nothing to rotate.
fn collect_due(state: &SharedState, cutoff: i64) -> anyhow::Result<Vec<(String, String)>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name FROM servers \
         WHERE kind = 'agent' \
           AND is_local = 0 \
           AND agent_token IS NOT NULL \
           AND agent_token <> '' \
           AND (token_rotated_at IS NULL OR token_rotated_at < ?1) \
         ORDER BY token_rotated_at NULLS FIRST, name",
    )?;
    let rows = stmt
        .query_map([cutoff], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}
