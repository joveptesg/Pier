//! Background scheduler that pulls projects + stacks from every peer
//! and refreshes the `federated_*` cache tables.
//!
//! Cadence: every `DEFAULT_INTERVAL_SECS` (default 45s) we walk the
//! peer list, do two concurrent `fetch_projects` / `fetch_stacks`
//! calls per peer, and replace the cache rows for that peer in one
//! transaction. A peer's failures don't stop us from refreshing the
//! others — that's the whole point of an aggregated view.
//!
//! Failure handling: we don't *delete* stale rows on failure. If vps2
//! is briefly unreachable, its last-known projects/stacks stay in the
//! cache and the UI surfaces "last sync 5m ago" so the operator knows
//! the data is frozen. Only a successful poll mutates the cache rows.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use tokio::time::{interval, Duration};

use crate::state::SharedState;

use super::client::{self, PeerEndpoint, RemoteProject, RemoteStack};

const DEFAULT_INTERVAL_SECS: u64 = 45;
const INITIAL_DELAY_SECS: u64 = 15;

pub fn start_scheduler(state: SharedState) {
    tokio::spawn(async move {
        // Wait briefly so migrations and other boot tasks settle before
        // we start hammering peers. Mirrors `alerts::start_scheduler`.
        tokio::time::sleep(Duration::from_secs(INITIAL_DELAY_SECS)).await;

        let interval_secs =
            read_interval_setting(&state).unwrap_or(DEFAULT_INTERVAL_SECS);
        let mut tick = interval(Duration::from_secs(interval_secs));
        loop {
            tick.tick().await;
            match run_sync_pass(&state).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::debug!(
                            "federation_sync: refreshed {count} peer(s)"
                        );
                    }
                }
                Err(e) => tracing::warn!("federation_sync pass failed: {e}"),
            }
        }
    });
}

fn read_interval_setting(state: &SharedState) -> Option<u64> {
    let db = state.db.lock().ok()?;
    let v: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'federation.sync_interval_secs'",
            [],
            |row| row.get(0),
        )
        .ok()?;
    v.parse().ok()
}

/// Returns the number of peers we attempted (whether or not they
/// succeeded). Useful for log noise control.
pub async fn run_sync_pass(state: &SharedState) -> Result<usize> {
    let peers = client::list_peer_endpoints(state)?;
    if peers.is_empty() {
        return Ok(0);
    }
    let attempted = peers.len();
    for peer in peers {
        let result = sync_one_peer(state, &peer).await;
        record_peer_outcome(state, &peer.id, &result);
        if let Err(e) = result {
            tracing::debug!("federation_sync: peer {} failed: {e}", peer.name);
        }
    }
    Ok(attempted)
}

async fn sync_one_peer(state: &SharedState, peer: &PeerEndpoint) -> Result<()> {
    let (projects, stacks) = tokio::try_join!(
        client::fetch_projects(peer),
        client::fetch_stacks(peer),
    )?;
    upsert_peer_cache(state, &peer.id, &projects, &stacks)?;
    Ok(())
}

fn upsert_peer_cache(
    state: &SharedState,
    peer_id: &str,
    projects: &[RemoteProject],
    stacks: &[RemoteStack],
) -> Result<()> {
    let now = now_secs();
    let mut db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
    let tx = db.transaction()?;

    // Snapshot semantics: drop everything we previously had for this peer,
    // then re-insert. Wrapped in a transaction so a partial replace can't
    // leave the UI showing half-old half-new data.
    tx.execute(
        "DELETE FROM federated_projects WHERE peer_server_id = ?1",
        [peer_id],
    )?;
    tx.execute(
        "DELETE FROM federated_stacks WHERE peer_server_id = ?1",
        [peer_id],
    )?;

    {
        let mut stmt = tx.prepare(
            "INSERT INTO federated_projects \
                 (peer_server_id, project_id, name, description, services_count, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for p in projects {
            // The `/projects` list endpoint doesn't return services_count
            // today; default to 0. Sync v2 can call `/projects/{id}` per
            // row but that's N+1 — skip for now.
            stmt.execute(rusqlite::params![
                peer_id,
                p.id,
                p.name,
                p.description,
                0i64,
                now,
            ])?;
        }
    }

    {
        let mut stmt = tx.prepare(
            "INSERT INTO federated_stacks \
                 (peer_server_id, stack_id, name, status, has_yaml, fetched_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for s in stacks {
            stmt.execute(rusqlite::params![
                peer_id,
                s.id,
                s.name,
                if s.status.is_empty() {
                    "unknown".to_string()
                } else {
                    s.status.clone()
                },
                if s.has_yaml { 1i64 } else { 0i64 },
                now,
            ])?;
        }
    }

    tx.commit()?;
    Ok(())
}

fn record_peer_outcome(state: &SharedState, peer_id: &str, result: &Result<()>) {
    let now = now_secs();
    let Ok(db) = state.db.lock() else { return };
    match result {
        Ok(()) => {
            let _ = db.execute(
                "INSERT INTO federation_sync_state \
                     (peer_server_id, last_synced_at, last_attempt_at, last_status, last_error, consecutive_failures) \
                 VALUES (?1, ?2, ?2, 'ok', NULL, 0) \
                 ON CONFLICT(peer_server_id) DO UPDATE SET \
                     last_synced_at = excluded.last_synced_at, \
                     last_attempt_at = excluded.last_attempt_at, \
                     last_status = 'ok', \
                     last_error = NULL, \
                     consecutive_failures = 0",
                rusqlite::params![peer_id, now],
            );
        }
        Err(e) => {
            let msg = e.to_string();
            // Trim — the UI surfaces this in a tooltip; a 4KB stack
            // trace makes the column unreadable and bloats the DB.
            let trimmed: String = msg.chars().take(500).collect();
            let _ = db.execute(
                "INSERT INTO federation_sync_state \
                     (peer_server_id, last_synced_at, last_attempt_at, last_status, last_error, consecutive_failures) \
                 VALUES (?1, NULL, ?2, 'error', ?3, 1) \
                 ON CONFLICT(peer_server_id) DO UPDATE SET \
                     last_attempt_at = excluded.last_attempt_at, \
                     last_status = 'error', \
                     last_error = excluded.last_error, \
                     consecutive_failures = federation_sync_state.consecutive_failures + 1",
                rusqlite::params![peer_id, now, trimmed],
            );
        }
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
