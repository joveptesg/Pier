//! HTTP client used by primary pier-core to drive **write operations**
//! on a peer-core through its `/api/v1/agent/*` surface (Этап 2.3 on
//! the peer side).
//!
//! Mirrors [`federation::client`] in shape — same mesh-IP preference,
//! same self-signed-cert tolerance, same shared `reqwest::Client`
//! construction — but a different auth header (`X-Pier-Federation`
//! plaintext, not `X-Pier-Peer-Token`) and a different method set
//! (deploy/down/restart vs the read-only list ones).
//!
//! Separation rationale: read federation already runs as a polling
//! scheduler on a slow tick. Write federation is per-user-action and
//! needs different timeouts (deploys can take minutes), so wrapping
//! both in one client would force concessions on either freshness or
//! deploy resilience.

// Verb-shaped methods below are consumed by the UI handlers in 2.6;
// drop this allow once those land.
#![allow(dead_code)]

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::Value;

use crate::state::SharedState;

/// Resolved write target: where to POST and which token to attach.
#[derive(Debug, Clone)]
pub struct WritePeer {
    pub server_id: String,
    pub name: String,
    /// Already mesh-IP-preferring base URL (https://10.42.0.x:8443 when
    /// mesh is active, otherwise the stored public URL).
    pub base_url: String,
    /// Plaintext federation token. Sent in `X-Pier-Federation` on every
    /// call — peer hashes and looks up in `federation_tokens`.
    pub token: String,
}

/// Resolve a peer's write endpoint. Returns `Ok(None)` when the peer
/// exists but has no federation_token paired yet, so callers can
/// surface a clean "not paired" error in the UI instead of a confusing
/// 500.
pub fn lookup_write_peer(state: &SharedState, server_id: &str) -> Result<Option<WritePeer>> {
    let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
    let row = db
        .query_row(
            "SELECT s.name, s.url, s.host, s.port, s.federation_token,
                    wp.assigned_ip, wp.status, wc.enabled
             FROM servers s
             LEFT JOIN wireguard_peers wp ON wp.server_id = s.id
             LEFT JOIN wireguard_config wc ON wc.id = 1
             WHERE s.id = ?1 AND s.kind = 'peer' AND s.is_local = 0",
            [server_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
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
    let Some(token) = token.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };

    let mesh_active = mesh_enabled.unwrap_or(0) == 1
        && mesh_status.as_deref() == Some("active")
        && mesh_ip.is_some();
    let base_url = if mesh_active {
        let ip = mesh_ip.expect("mesh_active guarantees Some");
        format!("https://{ip}:{port}")
    } else if let Some(u) = url.filter(|s| !s.is_empty()) {
        normalize_peer_url(&u)
    } else {
        format!("https://{host}:{port}")
    };

    Ok(Some(WritePeer {
        server_id: server_id.to_string(),
        name,
        base_url,
        token,
    }))
}

fn normalize_peer_url(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

/// Shared `reqwest::Client` factory. Generous timeout because deploys
/// pull images and can run for minutes; the peer-side helper has its
/// own bounded timeouts so we don't need to set one here too tightly.
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        // Same rationale as federation::client: peer-cores often run
        // with a self-signed cert that the primary's CA store doesn't
        // know about. The federation token is what authenticates the
        // channel, not TLS chain validation.
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(300))
        .build()
        .context("building federation write client")
}

async fn post<B: Serialize>(peer: &WritePeer, path: &str, body: &B) -> Result<Value> {
    let url = format!("{}/api/v1/agent{}", peer.base_url, path);
    let resp = client()?
        .post(&url)
        .header(
            crate::auth::federation::FEDERATION_HEADER,
            &peer.token,
        )
        .json(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!(
            "peer {} returned {} for {path}: {}",
            peer.name,
            status,
            payload
        ));
    }
    Ok(payload)
}

async fn get(peer: &WritePeer, path: &str) -> Result<Value> {
    let url = format!("{}/api/v1/agent{}", peer.base_url, path);
    let resp = client()?
        .get(&url)
        .header(
            crate::auth::federation::FEDERATION_HEADER,
            &peer.token,
        )
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!(
            "peer {} returned {} for {path}",
            peer.name,
            status
        ));
    }
    Ok(payload)
}

async fn delete(peer: &WritePeer, path: &str) -> Result<Value> {
    let url = format!("{}/api/v1/agent{}", peer.base_url, path);
    let resp = client()?
        .delete(&url)
        .header(
            crate::auth::federation::FEDERATION_HEADER,
            &peer.token,
        )
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    let status = resp.status();
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        return Err(anyhow!(
            "peer {} returned {} for {path}",
            peer.name,
            status
        ));
    }
    Ok(payload)
}

// ---------------------------------------------------------------------------
// Public verb-shaped methods. Match the peer's federation_agent router.
// ---------------------------------------------------------------------------

pub async fn list_stacks(peer: &WritePeer) -> Result<Value> {
    get(peer, "/stacks").await
}

pub async fn create_stack(peer: &WritePeer, name: &str, yaml: &str) -> Result<Value> {
    post(
        peer,
        "/stacks",
        &serde_json::json!({ "name": name, "yaml": yaml }),
    )
    .await
}

pub async fn get_stack(peer: &WritePeer, stack_id: &str) -> Result<Value> {
    get(peer, &format!("/stacks/{stack_id}")).await
}

pub async fn deploy_stack(peer: &WritePeer, stack_id: &str) -> Result<Value> {
    post(
        peer,
        &format!("/stacks/{stack_id}/deploy"),
        &serde_json::json!({}),
    )
    .await
}

pub async fn down_stack(peer: &WritePeer, stack_id: &str) -> Result<Value> {
    post(
        peer,
        &format!("/stacks/{stack_id}/down"),
        &serde_json::json!({}),
    )
    .await
}

pub async fn restart_stack(peer: &WritePeer, stack_id: &str) -> Result<Value> {
    post(
        peer,
        &format!("/stacks/{stack_id}/restart"),
        &serde_json::json!({}),
    )
    .await
}

pub async fn delete_stack(peer: &WritePeer, stack_id: &str) -> Result<Value> {
    delete(peer, &format!("/stacks/{stack_id}")).await
}

pub async fn release_stack(peer: &WritePeer, stack_id: &str) -> Result<Value> {
    post(
        peer,
        &format!("/release/{stack_id}"),
        &serde_json::json!({}),
    )
    .await
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
}
