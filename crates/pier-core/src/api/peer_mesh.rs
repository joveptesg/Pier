//! Peer-kind (core↔core) mesh — RESPONDER side.
//!
//! These endpoints are hit by a *remote* pier-core (the pairing initiator,
//! the "owner") over the peer-grant HTTPS channel (`X-Pier-Peer-Token`,
//! validated by the auth middleware exactly like `/peers/probe`). The
//! initiator side lives in [`crate::network::core_mesh`].
//!
//! Protocol (v1 — requires **mutual peering**: both cores already have each
//! other as a `servers(kind='peer')` row, so the responder can map an
//! incoming request back to the owner by `base_url`):
//!
//!   1. owner `GET  /peers/mesh/describe` → our nodes, so the owner can plan
//!      IPs across both cores from one subnet.
//!   2. owner `POST /peers/mesh/propose`  → the owner's IP plan + its keyed
//!      nodes. We adopt the subnet, enable+configure our own mesh with the
//!      assigned IPs (reusing `configure_mesh`, so our private keys never
//!      leave our nodes — PR1), store the owner's nodes as *external peers*,
//!      and return our now-keyed nodes.
//!   3. owner `POST /peers/mesh/teardown` → drop the owner's external peers
//!      and reconfigure.
//!
//! v1 simplification: our mesh must be DISABLED before a `propose` — we don't
//! merge two already-live meshes. The owner allocates every IP.

use std::collections::HashMap;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::network::wireguard::{ensure_core_uid, MeshConfig};
use crate::state::SharedState;

/// One mesh node — public information only, never a private key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeDescriptor {
    /// `<owning_core_uid>:<server_id>` — globally unique across cores.
    pub node_uid: String,
    pub name: String,
    pub public_key: Option<String>,
    /// `host:listen_port` reachable by the other core's nodes.
    pub endpoint: String,
    /// Mesh IP; empty string before the owner has assigned one.
    pub assigned_ip: String,
    /// The raw `server_id` on the owning core — the key the owner uses to
    /// address an IP assignment back to this node.
    pub server_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DescribeResponse {
    pub core_uid: String,
    pub subnet: String,
    pub listen_port: u16,
    pub mesh_enabled: bool,
    pub nodes: Vec<NodeDescriptor>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IpAssignment {
    pub server_id: String,
    pub assigned_ip: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProposeRequest {
    pub owner_core_uid: String,
    /// The owner's base URL — our correlation key to find the
    /// `servers(kind='peer')` row that represents the owner here.
    pub owner_base_url: String,
    pub subnet: String,
    pub listen_port: u16,
    pub keepalive: u16,
    pub owner_nodes: Vec<NodeDescriptor>,
    pub assignments: Vec<IpAssignment>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ProposeResponse {
    pub responder_core_uid: String,
    pub nodes: Vec<NodeDescriptor>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TeardownRequest {
    pub owner_core_uid: String,
    pub owner_base_url: String,
}

/// Scheme-insensitive, trailing-slash-insensitive base-URL compare. The
/// operator may register a peer as `http://x` or `https://x/`; either should
/// match the `owner_base_url` the initiator sends.
pub fn same_base_url(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.trim()
            .trim_end_matches('/')
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .to_ascii_lowercase()
    }
    norm(a) == norm(b)
}

/// Find the `servers(kind='peer')` row whose `url` matches `base_url`.
fn find_peer_by_base_url(
    db: &rusqlite::Connection,
    base_url: &str,
) -> anyhow::Result<Option<String>> {
    let mut stmt = db.prepare("SELECT id, COALESCE(url, '') FROM servers WHERE kind = 'peer'")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (id, url) = row?;
        if !url.is_empty() && same_base_url(&url, base_url) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// Build descriptors for THIS core's nodes (local + agents; never peers).
/// `assigned_ip`/`public_key` come from `wireguard_peers` when the mesh is up;
/// otherwise they're empty/None and the owner fills them via the IP plan.
pub(crate) fn local_node_descriptors(
    db: &rusqlite::Connection,
    core_uid: &str,
    listen_port: u16,
) -> anyhow::Result<Vec<NodeDescriptor>> {
    let mut stmt = db.prepare(
        "SELECT s.id, s.name, s.host, wp.assigned_ip, wp.public_key
         FROM servers s
         LEFT JOIN wireguard_peers wp ON wp.server_id = s.id
         WHERE COALESCE(s.kind, 'local') != 'peer'
         ORDER BY s.is_local DESC, s.created_at ASC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, name, host, assigned_ip, public_key) = row?;
        out.push(NodeDescriptor {
            node_uid: format!("{core_uid}:{id}"),
            name,
            public_key,
            endpoint: crate::network::address::authority(&host, listen_port.into()),
            assigned_ip: assigned_ip.unwrap_or_default(),
            server_id: id,
        });
    }
    Ok(out)
}

/// `GET /api/v1/peers/mesh/describe` — our mesh descriptor for an initiator.
pub async fn describe(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    let core_uid = ensure_core_uid(&db).map_err(AppError::Internal)?;
    let nodes =
        local_node_descriptors(&db, &core_uid, cfg.listen_port).map_err(AppError::Internal)?;
    Ok(Json(DescribeResponse {
        core_uid,
        subnet: cfg.subnet,
        listen_port: cfg.listen_port,
        mesh_enabled: cfg.enabled,
        nodes,
    }))
}

/// `POST /api/v1/peers/mesh/propose` — adopt the owner's plan, configure our
/// mesh, and return our keyed nodes.
pub async fn propose(
    State(state): State<SharedState>,
    Json(body): Json<ProposeRequest>,
) -> AppResult<impl IntoResponse> {
    let now = chrono::Utc::now().timestamp();

    // Phase A — validate + adopt the plan under a scoped lock (dropped before
    // the long-running configure round-trips).
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
        if cfg.enabled {
            return Err(AppError::Conflict(
                "disable this core's mesh before pairing (v1 does not merge live meshes)".into(),
            ));
        }
        let owner_id = find_peer_by_base_url(&db, &body.owner_base_url)
            .map_err(AppError::Internal)?
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "the initiating core ({}) is not registered as a peer here — \
                     add it under Servers as a peer first",
                    body.owner_base_url
                ))
            })?;
        let our_uid = ensure_core_uid(&db).map_err(AppError::Internal)?;

        let assign: HashMap<&str, &str> = body
            .assignments
            .iter()
            .map(|a| (a.server_id.as_str(), a.assigned_ip.as_str()))
            .collect();

        let tx = db
            .unchecked_transaction()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;

        tx.execute(
            "UPDATE wireguard_config
             SET subnet = ?1, listen_port = ?2, persistent_keepalive = ?3,
                 enabled = 1, core_uid = COALESCE(core_uid, ?4), updated_at = ?5
             WHERE id = 1",
            rusqlite::params![
                body.subnet,
                body.listen_port as i64,
                body.keepalive as i64,
                our_uid,
                now
            ],
        )?;

        // Allocate our nodes the owner-assigned IPs.
        let our_nodes: Vec<(String, String)> = {
            let mut stmt = tx.prepare(
                "SELECT id, host FROM servers
                 WHERE COALESCE(kind, 'local') != 'peer'
                 ORDER BY is_local DESC, created_at ASC",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        for (sid, host) in &our_nodes {
            let ip = assign.get(sid.as_str()).ok_or_else(|| {
                AppError::BadRequest(format!("owner sent no IP assignment for our node {sid}"))
            })?;
            let endpoint = crate::network::address::authority(host, body.listen_port.into());
            tx.execute(
                "INSERT OR REPLACE INTO wireguard_peers
                    (server_id, assigned_ip, endpoint, status, created_at)
                 VALUES (?1, ?2, ?3, 'pending', ?4)",
                rusqlite::params![sid, ip, endpoint, now],
            )?;
        }

        // Mark the owner peer row + (re)store the owner's nodes as externals.
        tx.execute(
            "UPDATE servers SET peer_core_uid = ?2 WHERE id = ?1",
            rusqlite::params![owner_id, body.owner_core_uid],
        )?;
        tx.execute(
            "DELETE FROM wireguard_external_peers WHERE peer_server_id = ?1",
            rusqlite::params![owner_id],
        )?;
        for n in &body.owner_nodes {
            tx.execute(
                "INSERT OR REPLACE INTO wireguard_external_peers
                    (node_uid, peer_server_id, name, public_key, endpoint, assigned_ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    n.node_uid,
                    owner_id,
                    n.name,
                    n.public_key,
                    n.endpoint,
                    n.assigned_ip
                ],
            )?;
        }
        tx.commit()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;
    }

    // Phase B — configure our own mesh. Reuses the PR1 path: each node mints
    // its keypair locally (private key never leaves the node) and the rendered
    // wg0.conf already lists the owner's external nodes.
    crate::api::network::configure_mesh(State(state.clone())).await?;

    // Phase C — return our now-keyed nodes so the owner can render us.
    let resp = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
        let our_uid = ensure_core_uid(&db).map_err(AppError::Internal)?;
        let nodes =
            local_node_descriptors(&db, &our_uid, cfg.listen_port).map_err(AppError::Internal)?;
        ProposeResponse {
            responder_core_uid: our_uid,
            nodes,
        }
    };
    Ok(Json(resp))
}

/// `POST /api/v1/peers/mesh/teardown` — drop the owner's external nodes and
/// reconfigure so our wg0.conf no longer lists them.
pub async fn teardown(
    State(state): State<SharedState>,
    Json(body): Json<TeardownRequest>,
) -> AppResult<impl IntoResponse> {
    let (removed, mesh_enabled) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
        let owner_id =
            find_peer_by_base_url(&db, &body.owner_base_url).map_err(AppError::Internal)?;
        let mut removed = false;
        if let Some(owner_id) = owner_id {
            db.execute(
                "DELETE FROM wireguard_external_peers WHERE peer_server_id = ?1",
                rusqlite::params![owner_id],
            )?;
            db.execute(
                "UPDATE servers SET peer_core_uid = NULL WHERE id = ?1",
                rusqlite::params![owner_id],
            )?;
            removed = true;
        }
        (removed, cfg.enabled)
    };

    // Re-render our nodes without the owner's [Peer] blocks (best-effort —
    // only when our mesh is up).
    if removed && mesh_enabled {
        let _ = crate::api::network::configure_mesh(State(state.clone())).await;
    }
    Ok(Json(serde_json::json!({ "ok": true, "removed": removed })))
}
