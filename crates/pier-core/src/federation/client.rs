//! HTTP client used by `federation::sync` to talk to remote peer-core
//! instances. Mirrors the auth + URL handling that `api::servers::proxy`
//! does inline, but exposes typed `fetch_projects` / `fetch_stacks`
//! helpers so the scheduler doesn't need to deal with `reqwest` directly.
//!
//! Auth: peer cores expect their grant token in the `X-Pier-Peer-Token`
//! header (see `auth::middleware::PEER_TOKEN_HEADER`). The token lives
//! in `servers.agent_token` for `kind='peer'` rows — when the operator
//! registers a peer in the UI, that field is set to a grant token the
//! peer minted on its side.
//!
//! Transport preference: when the WireGuard mesh is active and this peer
//! has an assigned mesh IP, we route through it the same way
//! `network::mesh_call::lookup_server` does for helper ops. This keeps
//! federation traffic off the public internet whenever the mesh is up.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::state::SharedState;

/// One peer's identifying info plus the resolved transport endpoint.
/// We *don't* expose the token outside this module — the caller passes
/// `peer_server_id` and we look it up here.
#[derive(Debug, Clone)]
pub struct PeerEndpoint {
    pub id: String,
    pub name: String,
    /// Whatever URL we should hit. `https://10.42.0.2:8443` when mesh
    /// is up, otherwise the stored public URL.
    pub base_url: String,
    /// Plaintext peer-grant token, sent in `X-Pier-Peer-Token`.
    pub token: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoteProject {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoteStack {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub has_yaml: bool,
}

/// Load every peer-kind server that's eligible for sync. Inactive /
/// soft-deleted rows are excluded; offline ones are *not* filtered out
/// here — the scheduler still wants to try them and update the
/// `consecutive_failures` counter.
pub fn list_peer_endpoints(state: &SharedState) -> Result<Vec<PeerEndpoint>> {
    let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT s.id, s.name, s.url, s.host, s.port, s.agent_token,
                wp.assigned_ip, wp.status, wc.enabled
         FROM servers s
         LEFT JOIN wireguard_peers wp ON wp.server_id = s.id
         LEFT JOIN wireguard_config wc ON wc.id = 1
         WHERE s.kind = 'peer' AND s.is_local = 0",
    )?;

    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<i64>>(8)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    let mut out = Vec::with_capacity(rows.len());
    for (id, name, url, host, port, token, mesh_ip, mesh_status, mesh_enabled) in rows {
        let mesh_active = mesh_enabled.unwrap_or(0) == 1
            && mesh_status.as_deref() == Some("active")
            && mesh_ip.is_some();
        let base_url = if mesh_active {
            // Mesh route: use the peer's assigned IP on the same port the
            // public URL exposes (default 8443). Scheme is https because
            // core's TLS still terminates on this socket — only the
            // transport network changed.
            let ip = mesh_ip.expect("mesh_active guarantees Some");
            format!("https://{}", crate::network::address::authority(&ip, port))
        } else if let Some(u) = url.filter(|s| !s.is_empty()) {
            normalize_peer_url(&u)
        } else {
            // Legacy rows with no url column populated — fall back to
            // (host, port) like the proxy handler did originally.
            format!(
                "https://{}",
                crate::network::address::authority(&host, port)
            )
        };
        out.push(PeerEndpoint {
            id,
            name,
            base_url,
            token,
        });
    }
    Ok(out)
}

/// Same idempotent normaliser the proxy handler uses; duplicated here
/// because making `api::servers::normalize_peer_url` public for one
/// caller would leak a private implementation detail.
fn normalize_peer_url(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Peers may use a self-signed cert that the operator already
        // accepted when registering the peer in the UI. Federation
        // pulls happen unattended every 30s — refusing self-signed
        // here would break every install that hasn't bought a real
        // cert for its peer-core endpoint. The peer-grant token is
        // what authenticates the channel, not the TLS chain.
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(15))
        .build()
        .context("building federation http client")
}

pub async fn fetch_projects(peer: &PeerEndpoint) -> Result<Vec<RemoteProject>> {
    let url = format!("{}/api/v1/projects", peer.base_url);
    let resp = client()?
        .get(&url)
        .header("X-Pier-Peer-Token", &peer.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "peer {} returned {} for /api/v1/projects",
            peer.name,
            resp.status()
        ));
    }
    resp.json().await.with_context(|| format!("decode {url}"))
}

pub async fn fetch_stacks(peer: &PeerEndpoint) -> Result<Vec<RemoteStack>> {
    let url = format!("{}/api/v1/stacks", peer.base_url);
    let resp = client()?
        .get(&url)
        .header("X-Pier-Peer-Token", &peer.token)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "peer {} returned {} for /api/v1/stacks",
            peer.name,
            resp.status()
        ));
    }
    resp.json().await.with_context(|| format!("decode {url}"))
}

/// Look up one peer endpoint by id. Returns Ok(None) if the row exists
/// but isn't a peer (so callers can produce a clean 404 instead of a
/// confusing 500). Currently unused — kept for the upcoming write-
/// federation phase (Etap 2) where per-peer mutation handlers need to
/// resolve an endpoint without scanning the full peer list.
#[allow(dead_code)]
pub fn lookup_peer(state: &SharedState, id: &str) -> Result<Option<PeerEndpoint>> {
    let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
    let row = db
        .query_row(
            "SELECT s.name, s.url, s.host, s.port, s.agent_token,
                    wp.assigned_ip, wp.status, wc.enabled
             FROM servers s
             LEFT JOIN wireguard_peers wp ON wp.server_id = s.id
             LEFT JOIN wireguard_config wc ON wc.id = 1
             WHERE s.id = ?1 AND s.kind = 'peer' AND s.is_local = 0",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((name, url, host, port, token, mesh_ip, mesh_status, mesh_enabled)) = row else {
        return Ok(None);
    };
    let mesh_active = mesh_enabled.unwrap_or(0) == 1
        && mesh_status.as_deref() == Some("active")
        && mesh_ip.is_some();
    let base_url = if mesh_active {
        let ip = mesh_ip.expect("mesh_active guarantees Some");
        format!("https://{}", crate::network::address::authority(&ip, port))
    } else if let Some(u) = url.filter(|s| !s.is_empty()) {
        normalize_peer_url(&u)
    } else {
        format!(
            "https://{}",
            crate::network::address::authority(&host, port)
        )
    };
    Ok(Some(PeerEndpoint {
        id: id.to_string(),
        name,
        base_url,
        token,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_upgrades_http() {
        assert_eq!(normalize_peer_url("http://x.example"), "https://x.example");
    }
    #[test]
    fn normalize_passes_https() {
        assert_eq!(normalize_peer_url("https://x.example"), "https://x.example");
    }
    #[test]
    fn normalize_passes_unknown_scheme() {
        // Some legacy installs might have stored a bare host. We don't
        // try to be clever — the proxy handler likewise leaves it.
        assert_eq!(normalize_peer_url("x.example:8443"), "x.example:8443");
    }
}
