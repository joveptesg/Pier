//! HTTP endpoints for WireGuard mesh management.
//!
//! Scope of this file:
//!
//! * `GET  /api/v1/network/mesh` — current mesh config + peers.
//! * `PUT  /api/v1/network/mesh` — change subnet/port (mesh disabled only).
//! * `POST /api/v1/network/mesh/enable` — allocate IPs for every server
//!   and flip `enabled=1`. Does NOT install WireGuard or push configs —
//!   that's the job of a future provision/apply pass.
//! * `POST /api/v1/network/mesh/disable` — drop every `wireguard_peers`
//!   row and flip `enabled=0`.
//!
//! Out of scope for now (lives in the next commit):
//!
//! * Driving `install_wireguard` / `generate_keypair` / `write_config`
//!   on each node via the agent's `/api/v1/agent/mesh/{op}` proxy.
//! * Switching `PIER_HOST` from public IP to the mesh IP.
//! * Auto-redeploying stacks with refreshed `extra_hosts`.
//!
//! Keeping the surface this thin makes the data model reviewable in
//! isolation; mistakes here only stall the wizard, they can't break a
//! live mesh because there isn't one yet.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::network::wireguard::{allocate_ip, MeshConfig, Peer, Subnet};
use crate::state::SharedState;

/// `GET /api/v1/network/mesh`
pub async fn get_mesh(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    let peers = Peer::load_all(&db).map_err(AppError::Internal)?;
    Ok(Json(serde_json::json!({
        "config": {
            "enabled": cfg.enabled,
            "subnet": cfg.subnet,
            "listen_port": cfg.listen_port,
            "persistent_keepalive": cfg.persistent_keepalive,
            "updated_at": cfg.updated_at,
        },
        "peers": peers.iter().map(|p| serde_json::json!({
            "server_id": p.server_id,
            "server_name": p.server_name,
            "is_local": p.is_local,
            "assigned_ip": p.assigned_ip.to_string(),
            "public_key": p.public_key,
            "endpoint": p.endpoint,
            "status": p.status,
            "error_message": p.error_message,
            "last_handshake": p.last_handshake,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct UpdateMeshRequest {
    /// CIDR. Validated server-side; only /30 or shorter prefixes allowed.
    #[serde(default)]
    pub subnet: Option<String>,
    /// UDP port for the WireGuard listener on each node.
    #[serde(default)]
    pub listen_port: Option<u16>,
    /// 0 to disable keepalive entirely (only safe if no node is behind NAT).
    #[serde(default)]
    pub persistent_keepalive: Option<u16>,
}

/// `PUT /api/v1/network/mesh`
///
/// Changing the subnet while peers exist would orphan their assigned IPs
/// and require a full re-roll, so we refuse mutation while `enabled=1`
/// or while any `wireguard_peers` rows survive. The UI surfaces this as
/// "disable mesh first to change subnet."
pub async fn put_mesh(
    State(state): State<SharedState>,
    Json(body): Json<UpdateMeshRequest>,
) -> AppResult<impl IntoResponse> {
    if let Some(ref s) = body.subnet {
        // Validate up front so we don't write garbage to the DB.
        Subnet::parse(s).map_err(|e| AppError::BadRequest(format!("subnet: {e}")))?;
    }
    if let Some(p) = body.listen_port {
        if p < 1024 {
            return Err(AppError::BadRequest(
                "listen_port must be ≥1024 (privileged ports not allowed)".into(),
            ));
        }
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    let peer_count: i64 = db.query_row("SELECT COUNT(*) FROM wireguard_peers", [], |r| r.get(0))?;
    if cfg.enabled || peer_count > 0 {
        return Err(AppError::BadRequest(
            "mesh must be disabled and have no peers before config can change".into(),
        ));
    }

    let now = chrono::Utc::now().timestamp();
    db.execute(
        "UPDATE wireguard_config
         SET subnet               = COALESCE(?1, subnet),
             listen_port          = COALESCE(?2, listen_port),
             persistent_keepalive = COALESCE(?3, persistent_keepalive),
             updated_at           = ?4
         WHERE id = 1",
        rusqlite::params![
            body.subnet,
            body.listen_port.map(|p| p as i64),
            body.persistent_keepalive.map(|k| k as i64),
            now,
        ],
    )?;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// `POST /api/v1/network/mesh/enable`
///
/// Allocates a private /32 to every server (local + agents + peers) and
/// flips `enabled=1`. The next step — actually installing WireGuard on
/// each node and pushing wg0.conf — is a separate request so the
/// operator can review the IP plan before anything privileged runs.
pub async fn enable_mesh(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    if cfg.enabled {
        return Err(AppError::BadRequest("mesh is already enabled".into()));
    }
    let subnet = Subnet::parse(&cfg.subnet).map_err(|e| {
        AppError::Internal(anyhow::anyhow!(
            "stored subnet {} is invalid: {e}",
            cfg.subnet
        ))
    })?;

    // Allocate inside a single transaction so a partial failure leaves
    // the DB in a clean "enabled=0, no peers" state.
    let tx = db
        .unchecked_transaction()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;

    // Already-allocated IPs (should be empty since we refused PUT while
    // peers exist, but read anyway to be defensive).
    let mut used: Vec<std::net::Ipv4Addr> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT assigned_ip FROM wireguard_peers")?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
            if let Ok(ip) = row?.parse() {
                used.push(ip);
            }
        }
    }

    // List every server eligible for the mesh. `local` always gets the
    // first host so the operator can hard-code `https://10.42.0.1:PORT`
    // in env files and runbooks.
    let mut servers: Vec<(String, String, bool)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, host, is_local FROM servers ORDER BY is_local DESC, created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        for row in rows {
            servers.push(row?);
        }
    }
    // Defensive sort: local first, others in deterministic order. The
    // `ORDER BY is_local DESC` already does this, but if a future migration
    // adds more `local` rows we don't want surprises.
    servers.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    let now = chrono::Utc::now().timestamp();
    for (server_id, host, _is_local) in &servers {
        let ip = allocate_ip(&subnet, &used).map_err(AppError::Internal)?;
        used.push(ip);
        let endpoint = format!("{}:{}", host, cfg.listen_port);
        tx.execute(
            "INSERT INTO wireguard_peers
                (server_id, assigned_ip, endpoint, status, created_at)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
            rusqlite::params![server_id, ip.to_string(), endpoint, now],
        )?;
    }

    tx.execute(
        "UPDATE wireguard_config SET enabled = 1, updated_at = ?1 WHERE id = 1",
        rusqlite::params![now],
    )?;
    tx.commit()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "peers_allocated": servers.len(),
    })))
}

/// `POST /api/v1/network/mesh/disable`
///
/// Drops every `wireguard_peers` row and flips `enabled=0`. Does NOT
/// `wg-quick down` anything — that's handled by a separate teardown
/// pass once we've wired the agent mesh proxy into orchestration. For
/// now disabling is a DB-only operation, suitable for clearing a bad
/// allocation before any real apply has happened.
pub async fn disable_mesh(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let tx = db
        .unchecked_transaction()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;
    tx.execute("DELETE FROM wireguard_peers", [])?;
    let now = chrono::Utc::now().timestamp();
    tx.execute(
        "UPDATE wireguard_config SET enabled = 0, updated_at = ?1 WHERE id = 1",
        rusqlite::params![now],
    )?;
    tx.commit()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;
    Ok(Json(serde_json::json!({"ok": true})))
}
