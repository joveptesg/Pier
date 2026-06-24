//! Peer-kind (core↔core) mesh — INITIATOR side.
//!
//! Drives the pairing handshake against a remote core's
//! [`crate::api::peer_mesh`] endpoints over the peer-grant HTTPS channel
//! (`X-Pier-Peer-Token`). The local core ("owner") allocates every mesh IP
//! from its own subnet, ships the plan to the remote core, learns the remote
//! core's now-keyed nodes, stores them as external peers, and re-renders its
//! own `wg0.conf` so its nodes gain `[Peer]` blocks for the remote nodes.
//!
//! v1 requires mutual peering (each core registered the other as a
//! `servers(kind='peer')` row) and that BOTH meshes are owner-allocated: the
//! remote core must have its mesh DISABLED before pairing.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::extract::State;
use rusqlite::OptionalExtension;

use crate::api::peer_mesh::{
    local_node_descriptors, DescribeResponse, IpAssignment, ProposeRequest, ProposeResponse,
    TeardownRequest,
};
use crate::auth::middleware::PEER_TOKEN_HEADER;
use crate::error::{AppError, AppResult};
use crate::network::wireguard::{allocate_ip, ensure_core_uid, MeshConfig, Subnet};
use crate::state::SharedState;

/// The remote core configures its whole mesh (generating a keypair per node)
/// inside the `propose` call, so this client tolerates a long round-trip.
const PAIR_TIMEOUT: Duration = Duration::from_secs(180);

fn peer_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Peer cores serve self-signed panel certs the operator already
        // accepted when registering the peer; the grant token authenticates
        // the channel, exactly like federation pulls.
        .danger_accept_invalid_certs(true)
        .timeout(PAIR_TIMEOUT)
        .build()
        .context("building peer-mesh http client")
}

/// Idempotent `http://` → `https://`, trailing-slash trim.
fn normalize(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if let Some(rest) = u.strip_prefix("http://") {
        format!("https://{rest}")
    } else if u.starts_with("https://") {
        u.to_string()
    } else {
        format!("https://{u}")
    }
}

/// `(base_url, peer_grant_token)` for an A→B `servers(kind='peer')` row.
fn peer_endpoint(db: &rusqlite::Connection, peer_id: &str) -> AppResult<(String, String)> {
    let (url, token): (Option<String>, String) = db
        .query_row(
            "SELECT url, agent_token FROM servers WHERE id = ?1 AND kind = 'peer'",
            [peer_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(|e| AppError::Internal(anyhow!("peer lookup: {e}")))?
        .ok_or_else(|| AppError::NotFound(format!("peer server {peer_id} not found")))?;
    let url = url
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest(format!("peer {peer_id} has no URL set")))?;
    Ok((normalize(&url), token))
}

/// This core's own base URL, the way a peer core would register it.
fn own_base_url(db: &rusqlite::Connection, port: u16) -> AppResult<String> {
    let ip: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'server.public_ipv4'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .map_err(|e| AppError::Internal(anyhow!("settings lookup: {e}")))?
        .or_else(|| {
            db.query_row(
                "SELECT value FROM settings WHERE key = 'server.public_ip'",
                [],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
        })
        .filter(|s| !s.is_empty());
    let ip = ip.ok_or_else(|| {
        AppError::BadRequest(
            "this core's public IP is unknown (settings server.public_ipv4) — \
             set it so the peer can map us back"
                .into(),
        )
    })?;
    Ok(format!(
        "https://{}",
        crate::network::address::authority(&ip, port.into())
    ))
}

/// `POST /api/v1/network/mesh/pair/{peer_id}` — pair our mesh with peer core
/// `peer_id`.
pub async fn pair(state: &SharedState, peer_id: &str) -> AppResult<serde_json::Value> {
    // Snapshot everything we need under one scoped lock.
    let (
        subnet_str,
        listen_port,
        keepalive,
        our_uid,
        base_url,
        owner_nodes,
        peer_url,
        peer_token,
        mut used,
    ) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
        if !cfg.enabled {
            return Err(AppError::BadRequest(
                "enable and configure this core's own mesh before pairing".into(),
            ));
        }
        let our_uid = ensure_core_uid(&db).map_err(AppError::Internal)?;
        let base_url = own_base_url(&db, state.config.port)?;
        let owner_nodes =
            local_node_descriptors(&db, &our_uid, cfg.listen_port).map_err(AppError::Internal)?;
        if owner_nodes
            .iter()
            .any(|n| n.public_key.is_none() || n.assigned_ip.is_empty())
        {
            return Err(AppError::BadRequest(
                "configure this core's mesh first — some local nodes are not keyed yet".into(),
            ));
        }
        let (peer_url, peer_token) = peer_endpoint(&db, peer_id)?;

        let mut used: Vec<std::net::Ipv4Addr> = Vec::new();
        for sql in [
            "SELECT assigned_ip FROM wireguard_peers",
            "SELECT assigned_ip FROM wireguard_external_peers",
        ] {
            let mut stmt = db.prepare(sql)?;
            for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
                if let Ok(ip) = row?.parse() {
                    used.push(ip);
                }
            }
        }
        (
            cfg.subnet,
            cfg.listen_port,
            cfg.persistent_keepalive,
            our_uid,
            base_url,
            owner_nodes,
            peer_url,
            peer_token,
            used,
        )
    };

    let subnet = Subnet::parse(&subnet_str).map_err(AppError::Internal)?;
    let client = peer_client().map_err(AppError::Internal)?;

    // 1. describe — learn the remote core's nodes (mesh must be off there).
    let desc: DescribeResponse = client
        .get(format!("{peer_url}/api/v1/peers/mesh/describe"))
        .header(PEER_TOKEN_HEADER, &peer_token)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("describe: peer unreachable: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::BadRequest(format!("describe rejected: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow!("decode describe: {e}")))?;

    if desc.core_uid == our_uid {
        return Err(AppError::BadRequest(
            "refusing to pair a core with itself".into(),
        ));
    }
    if desc.mesh_enabled {
        return Err(AppError::Conflict(
            "the target core's mesh is enabled; disable it before pairing (v1)".into(),
        ));
    }

    // 2. allocate IPs for the remote nodes from our subnet.
    let mut assignments = Vec::with_capacity(desc.nodes.len());
    for n in &desc.nodes {
        let ip = allocate_ip(&subnet, &used)
            .map_err(|e| AppError::BadRequest(format!("IP allocation for remote nodes: {e}")))?;
        used.push(ip);
        assignments.push(IpAssignment {
            server_id: n.server_id.clone(),
            assigned_ip: ip.to_string(),
        });
    }

    // 3. propose — the remote core adopts the plan, configures, returns keyed.
    let req = ProposeRequest {
        owner_core_uid: our_uid.clone(),
        owner_base_url: base_url,
        subnet: subnet_str,
        listen_port,
        keepalive,
        owner_nodes,
        assignments,
    };
    let presp: ProposeResponse = client
        .post(format!("{peer_url}/api/v1/peers/mesh/propose"))
        .header(PEER_TOKEN_HEADER, &peer_token)
        .json(&req)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("propose: peer unreachable: {e}")))?
        .error_for_status()
        .map_err(|e| AppError::BadRequest(format!("propose rejected: {e}")))?
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow!("decode propose: {e}")))?;

    // 4. persist the remote core's nodes as our external peers.
    let stored = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE servers SET peer_core_uid = ?2 WHERE id = ?1",
            rusqlite::params![peer_id, presp.responder_core_uid],
        )?;
        db.execute(
            "DELETE FROM wireguard_external_peers WHERE peer_server_id = ?1",
            rusqlite::params![peer_id],
        )?;
        let mut stored = 0usize;
        for n in &presp.nodes {
            if n.public_key.is_none() || n.assigned_ip.is_empty() {
                continue; // remote node not keyed — skip; a re-pair will pick it up
            }
            db.execute(
                "INSERT OR REPLACE INTO wireguard_external_peers
                    (node_uid, peer_server_id, name, public_key, endpoint, assigned_ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    n.node_uid,
                    peer_id,
                    n.name,
                    n.public_key,
                    n.endpoint,
                    n.assigned_ip
                ],
            )?;
            stored += 1;
        }
        stored
    };

    // 5. re-render our own nodes so they gain [Peer] blocks for the remote.
    crate::api::network::configure_mesh(State(state.clone())).await?;

    Ok(serde_json::json!({
        "ok": true,
        "paired_core_uid": presp.responder_core_uid,
        "remote_nodes": stored,
    }))
}

/// `POST /api/v1/network/mesh/peer/{peer_id}/unpair` — tear the pairing down
/// on both sides (remote teardown is best-effort) and reconfigure locally.
pub async fn unpair(state: &SharedState, peer_id: &str) -> AppResult<serde_json::Value> {
    let (our_uid, base_url, peer_url, peer_token, mesh_enabled) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
        let our_uid = ensure_core_uid(&db).map_err(AppError::Internal)?;
        let base_url = own_base_url(&db, state.config.port).unwrap_or_default();
        let (peer_url, peer_token) = peer_endpoint(&db, peer_id)?;
        (our_uid, base_url, peer_url, peer_token, cfg.enabled)
    };

    // Best-effort remote teardown — proceed locally even if the peer is down.
    if let Ok(client) = peer_client() {
        let _ = client
            .post(format!("{peer_url}/api/v1/peers/mesh/teardown"))
            .header(PEER_TOKEN_HEADER, &peer_token)
            .json(&TeardownRequest {
                owner_core_uid: our_uid,
                owner_base_url: base_url,
            })
            .send()
            .await;
    }

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "DELETE FROM wireguard_external_peers WHERE peer_server_id = ?1",
            rusqlite::params![peer_id],
        )?;
        db.execute(
            "UPDATE servers SET peer_core_uid = NULL WHERE id = ?1",
            rusqlite::params![peer_id],
        )?;
    }

    if mesh_enabled {
        let _ = crate::api::network::configure_mesh(State(state.clone())).await;
    }
    Ok(serde_json::json!({ "ok": true }))
}
