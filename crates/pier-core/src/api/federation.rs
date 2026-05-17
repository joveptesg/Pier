//! Aggregated read-only views over local + cached peer projects/stacks.
//!
//! Endpoints (all behind the regular session auth, no peer-grant token
//! involved — these are *served by* the primary core to its own UI):
//!
//! * `GET /api/v1/federation/projects` — merged list. Local rows are
//!   tagged `source: "local"` with `peer_server_id: null`; cached peer
//!   rows carry `source: "peer"`, the peer's name/id, and the
//!   `fetched_at` of the last successful sync.
//! * `GET /api/v1/federation/stacks` — same, for stacks.
//! * `GET /api/v1/federation/status` — per-peer sync bookkeeping so
//!   the dashboard can show "vps2 last synced 32s ago" / errors.
//! * `POST /api/v1/federation/sync` — kick off an out-of-band refresh.
//!   The scheduler still ticks normally; this is for the "I just added
//!   a peer, show me its data now" button.
//!
//! Note: `/api/v1/federation/peer/{id}/proxy/{*rest}` is *not* added
//! here. The existing `/api/v1/servers/{id}/proxy/{*rest}` already
//! does that job for peer-kind servers; adding a second route would
//! be redundant. UI links use the existing path.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;

use crate::error::{AppError, AppResult};
use crate::federation::{sync, write_client};
use crate::state::SharedState;

/// GET /api/v1/federation/projects
///
/// Returns local projects first, then federated entries grouped by
/// peer. The UI relies on the `source` tag to pick whether to link to
/// `/projects/{id}` (local) or to open the peer-side UI (federated).
pub async fn list_projects(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut local_stmt = db.prepare(
        "SELECT id, name, description, port_range_start, port_range_end, created_at \
         FROM projects ORDER BY name",
    )?;
    let local: Vec<serde_json::Value> = local_stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "source": "local",
                "peer_server_id": serde_json::Value::Null,
                "peer_name": serde_json::Value::Null,
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "description": row.get::<_, String>(2)?,
                "port_range_start": row.get::<_, Option<i64>>(3)?,
                "port_range_end": row.get::<_, Option<i64>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut peer_stmt = db.prepare(
        "SELECT fp.peer_server_id, s.name, s.url, fp.project_id, fp.name, fp.description, \
                fp.services_count, fp.fetched_at \
         FROM federated_projects fp \
         JOIN servers s ON s.id = fp.peer_server_id \
         ORDER BY s.name, fp.name",
    )?;
    let peers: Vec<serde_json::Value> = peer_stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "source": "peer",
                "peer_server_id": row.get::<_, String>(0)?,
                "peer_name": row.get::<_, String>(1)?,
                "peer_url": row.get::<_, Option<String>>(2)?,
                "id": row.get::<_, String>(3)?,
                "name": row.get::<_, String>(4)?,
                "description": row.get::<_, String>(5)?,
                "services_count": row.get::<_, i64>(6)?,
                "fetched_at": row.get::<_, i64>(7)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut all = local;
    all.extend(peers);
    Ok(Json(all))
}

/// GET /api/v1/federation/stacks
pub async fn list_stacks(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut local_stmt = db.prepare(
        "SELECT id, name, compose_content, status, created_at \
         FROM services WHERE service_type = 'compose' ORDER BY name",
    )?;
    let local: Vec<serde_json::Value> = local_stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "source": "local",
                "peer_server_id": serde_json::Value::Null,
                "peer_name": serde_json::Value::Null,
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "has_yaml": row.get::<_, Option<String>>(2)?.is_some(),
                "status": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut peer_stmt = db.prepare(
        "SELECT fs.peer_server_id, s.name, s.url, fs.stack_id, fs.name, fs.status, fs.has_yaml, fs.fetched_at, \
                CASE WHEN s.federation_token IS NULL OR s.federation_token = '' \
                     THEN 0 ELSE 1 END AS peer_paired \
         FROM federated_stacks fs \
         JOIN servers s ON s.id = fs.peer_server_id \
         ORDER BY s.name, fs.name",
    )?;
    let peers: Vec<serde_json::Value> = peer_stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "source": "peer",
                "peer_server_id": row.get::<_, String>(0)?,
                "peer_name": row.get::<_, String>(1)?,
                "peer_url": row.get::<_, Option<String>>(2)?,
                "id": row.get::<_, String>(3)?,
                "name": row.get::<_, String>(4)?,
                "status": row.get::<_, String>(5)?,
                "has_yaml": row.get::<_, i64>(6)? != 0,
                "fetched_at": row.get::<_, i64>(7)?,
                "peer_paired": row.get::<_, i64>(8)? != 0,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let mut all = local;
    all.extend(peers);
    Ok(Json(all))
}

/// GET /api/v1/federation/status
///
/// Returns one row per peer-kind server with the last-known sync
/// outcome, or `last_status: "pending"` if the scheduler hasn't
/// reached the peer yet. The UI uses this to render the "12s ago"
/// freshness badge and the offline warning banner.
pub async fn status(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT s.id, s.name, s.kind, s.status, \
                fss.last_synced_at, fss.last_attempt_at, fss.last_status, \
                fss.last_error, fss.consecutive_failures \
         FROM servers s \
         LEFT JOIN federation_sync_state fss ON fss.peer_server_id = s.id \
         WHERE s.kind = 'peer' AND s.is_local = 0 \
         ORDER BY s.name",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "peer_server_id": row.get::<_, String>(0)?,
                "peer_name": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "reachable_status": row.get::<_, String>(3)?,
                "last_synced_at": row.get::<_, Option<i64>>(4)?,
                "last_attempt_at": row.get::<_, Option<i64>>(5)?,
                "last_status": row.get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "pending".to_string()),
                "last_error": row.get::<_, Option<String>>(7)?,
                "consecutive_failures": row.get::<_, Option<i64>>(8)?.unwrap_or(0),
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(rows))
}

/// POST /api/v1/federation/sync
///
/// Out-of-band refresh trigger. Runs one full pass synchronously and
/// returns when every peer has either succeeded or failed once.
/// Useful immediately after a peer registration; the scheduler will
/// take up to `interval_secs` to notice the new row otherwise.
pub async fn refresh_now(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let attempted = sync::run_sync_pass(&state)
        .await
        .map_err(|e| crate::error::AppError::Internal(anyhow::anyhow!(e)))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "peers_attempted": attempted,
    })))
}

// ---------------------------------------------------------------------------
// Write-federation passthroughs (Etap 2.6).
//
// Thin handlers that resolve the peer + token via write_client and forward
// the call. We deliberately keep these primary-side endpoints under
// /api/v1/federation/peer/{id}/... so they sit alongside the read views
// and inherit the same session-auth layer — only **primary's operator**
// can trigger them. The peer↔primary X-Pier-Federation hop happens
// inside write_client.
//
// Same pattern across deploy/down/restart/release: look up endpoint,
// call write_client verb, map errors to 4xx so the UI can surface them.
// ---------------------------------------------------------------------------

async fn resolve_peer(
    state: &SharedState,
    server_id: &str,
) -> AppResult<write_client::WritePeer> {
    write_client::lookup_write_peer(state, server_id)
        .map_err(|e| AppError::Internal(anyhow::anyhow!(e)))?
        .ok_or_else(|| {
            AppError::BadRequest(format!(
                "peer {server_id} is not paired for federation — set its token in /servers/<id>"
            ))
        })
}

/// POST /api/v1/federation/peer/{server_id}/stacks/{stack_id}/deploy
pub async fn peer_deploy_stack(
    State(state): State<SharedState>,
    Path((server_id, stack_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let peer = resolve_peer(&state, &server_id).await?;
    let res = write_client::deploy_stack(&peer, &stack_id)
        .await
        .map_err(|e| AppError::BadRequest(format!("peer rejected deploy: {e:#}")))?;
    let _ = sync::run_sync_pass(&state).await; // refresh cache so UI shows new state
    Ok(Json(res))
}

/// POST /api/v1/federation/peer/{server_id}/stacks/{stack_id}/down
pub async fn peer_down_stack(
    State(state): State<SharedState>,
    Path((server_id, stack_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let peer = resolve_peer(&state, &server_id).await?;
    let res = write_client::down_stack(&peer, &stack_id)
        .await
        .map_err(|e| AppError::BadRequest(format!("peer rejected down: {e:#}")))?;
    let _ = sync::run_sync_pass(&state).await;
    Ok(Json(res))
}

/// POST /api/v1/federation/peer/{server_id}/stacks/{stack_id}/restart
pub async fn peer_restart_stack(
    State(state): State<SharedState>,
    Path((server_id, stack_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let peer = resolve_peer(&state, &server_id).await?;
    let res = write_client::restart_stack(&peer, &stack_id)
        .await
        .map_err(|e| AppError::BadRequest(format!("peer rejected restart: {e:#}")))?;
    let _ = sync::run_sync_pass(&state).await;
    Ok(Json(res))
}

/// POST /api/v1/federation/peer/{server_id}/stacks/{stack_id}/release
pub async fn peer_release_stack(
    State(state): State<SharedState>,
    Path((server_id, stack_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let peer = resolve_peer(&state, &server_id).await?;
    let res = write_client::release_stack(&peer, &stack_id)
        .await
        .map_err(|e| AppError::BadRequest(format!("peer rejected release: {e:#}")))?;
    let _ = sync::run_sync_pass(&state).await;
    Ok(Json(res))
}
