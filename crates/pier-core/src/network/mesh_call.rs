//! Dispatch a mesh op to a server's `pier-net-helper`, regardless of
//! whether that server is local or remote.
//!
//! Local node (`servers.is_local = 1`): pier-core opens
//! `/run/pier/net.sock` directly. It can do this because the helper's
//! systemd unit creates the socket with mode `0660 root:pier`, and core
//! runs as user `pier`.
//!
//! Remote node (kind `agent`): pier-core POSTs to
//! `https://{host}:{port}/api/v1/agent/mesh/{op}` with the long-term
//! agent_token in `Authorization: Bearer …`. The agent's `mesh_proxy`
//! handler forwards the call into its own helper and returns the helper's
//! reply verbatim, so callers see the same `(ok, result, error)` shape
//! either way.
//!
//! Remote node (kind `peer`): not yet supported here — peer-to-peer mesh
//! orchestration requires deciding which side authoritatively writes
//! configs, and that's a Phase 0.3d concern. Calls return a clear error.

#![allow(dead_code)] // some entry points are used only by 0.3c-pending handlers

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use serde_json::Value;

use crate::state::SharedState;

/// Same envelope the helper sends back on the unix socket and that
/// `pier-agent::mesh_proxy` re-emits as JSON. `ok = false` means the
/// op ran but the helper refused (validation error, exit non-zero,
/// etc); the transport itself succeeded. Reachability/timeout problems
/// surface as `Err(_)` instead.
pub struct MeshOpResult {
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

const HELPER_SOCKET_DEFAULT: &str = "/run/pier/net.sock";

/// Top-level entry point. Dispatches `op` against the server identified
/// by `server_id`. `params` is anything `serde_json::to_value` accepts —
/// pass `&serde_json::json!({})` for unit ops.
pub async fn dispatch<P: Serialize>(
    state: &SharedState,
    server_id: &str,
    op: &str,
    params: &P,
) -> Result<MeshOpResult> {
    let (kind, host, port, agent_token, is_local, tls_fingerprint) =
        lookup_server(state, server_id)?;

    if is_local {
        return call_local_socket(op, params).await;
    }

    match kind.as_str() {
        "agent" => {
            call_remote_agent(
                &host,
                port,
                &agent_token,
                tls_fingerprint.as_deref(),
                op,
                params,
            )
            .await
        }
        "peer" => Err(anyhow!(
            "helper ops are never dispatched to a peer-kind server — a peer core \
             owns its own helpers and joins via the core↔core pairing protocol \
             (/network/mesh/pair), not direct dispatch (server_id={server_id})"
        )),
        other => Err(anyhow!("unknown server kind {other:?} for {server_id}")),
    }
}

fn lookup_server(
    state: &SharedState,
    server_id: &str,
) -> Result<(String, String, i64, String, bool, Option<String>)> {
    let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
    // Same mesh-preference logic as servers::get_server_info: once a
    // peer's wireguard_peers.status is `active`, subsequent mesh ops
    // go through its private IP. During the initial configure pass
    // every peer is still `pending` or `keyed`, so the first round
    // legitimately uses the public host — this is intentional, it's
    // how the bootstrap chain ever gets off the ground.
    let row = db
        .query_row(
            "SELECT s.kind, s.host, s.port, s.agent_token, s.is_local,
                    wp.assigned_ip, wp.status, wc.enabled, s.agent_tls_fingerprint
             FROM servers s
             LEFT JOIN wireguard_peers wp ON wp.server_id = s.id
             LEFT JOIN wireguard_config wc ON wc.id = 1
             WHERE s.id = ?1",
            [server_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? != 0,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow!("server {server_id} not found"))?;

    let (
        kind,
        mut host,
        port,
        token,
        is_local,
        mesh_ip,
        mesh_status,
        mesh_enabled,
        tls_fingerprint,
    ) = row;
    let mesh_active = mesh_enabled.unwrap_or(0) == 1
        && mesh_status.as_deref() == Some("active")
        && mesh_ip.is_some();
    if mesh_active && !is_local {
        if let Some(ip) = mesh_ip {
            host = ip;
        }
    }
    Ok((kind, host, port, token, is_local, tls_fingerprint))
}

// ---------------------------------------------------------------------------
// Local — direct unix-socket conversation with our own helper.
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn call_local_socket<P: Serialize>(op: &str, params: &P) -> Result<MeshOpResult> {
    use std::path::PathBuf;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let socket_path = std::env::var("PIER_NET_HELPER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(HELPER_SOCKET_DEFAULT));

    let body = build_envelope(op, params)?;
    let mut bytes = serde_json::to_vec(&body)?;
    bytes.push(b'\n');

    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&socket_path))
        .await
        .with_context(|| format!("connecting to {}", socket_path.display()))?
        .with_context(|| format!("connecting to {}", socket_path.display()))?;

    let (read_half, mut write_half) = stream.into_split();
    tokio::time::timeout(Duration::from_secs(120), async move {
        write_half.write_all(&bytes).await?;
        write_half.shutdown().await?;
        anyhow::Ok(())
    })
    .await
    .map_err(|_| anyhow!("helper write timed out"))??;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(120), reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("helper read timed out"))?
        .context("reading helper response")?;
    parse_wire(&line)
}

#[cfg(not(unix))]
async fn call_local_socket<P: Serialize>(_op: &str, _params: &P) -> Result<MeshOpResult> {
    Err(anyhow!(
        "local helper socket is Linux-only; running mesh ops on a non-unix host is not supported"
    ))
}

// ---------------------------------------------------------------------------
// Remote — POST to the agent's `/api/v1/agent/mesh/{op}` proxy.
// ---------------------------------------------------------------------------

async fn call_remote_agent<P: Serialize>(
    host: &str,
    port: i64,
    agent_token: &str,
    fingerprint: Option<&str>,
    op: &str,
    params: &P,
) -> Result<MeshOpResult> {
    // Sanity-check the op string up front so we don't even try to make
    // a URL with weird characters. The helper's whitelist is the
    // authoritative gate, but rejecting obvious garbage here makes
    // failures cheaper.
    if !op
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow!(
            "rejecting suspicious op string {op:?}: only [a-zA-Z0-9_-] allowed"
        ));
    }

    let url = format!(
        "https://{}/api/v1/agent/mesh/{op}",
        super::address::authority(host, port)
    );
    // > helper's 120s op timeout. Pinned to the agent's leaf fingerprint.
    let client = super::agent_client::build_agent_client(fingerprint, Duration::from_secs(150))
        .context("building pinned agent client")?;

    let body = serde_json::to_value(params).context("serializing params")?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;

    // The agent wraps the helper's reply in {"ok":bool, "result":…, "error":…}
    // identical to MeshOpResult, so we can deserialize straight to that.
    let status = resp.status();
    let payload: Value = resp
        .json()
        .await
        .with_context(|| format!("parsing agent response from {url}"))?;

    // Two failure modes:
    //   * Transport-ish (4xx/5xx from agent itself, e.g. unauthorized,
    //     helper unreachable on the remote node). Surface as Err so
    //     callers can distinguish "couldn't ask" from "asked and got
    //     refused".
    //   * Helper-level (200 OK with ok=false). Surface as Ok(result with
    //     ok=false) so the orchestrator can record the error per peer.
    if !status.is_success() {
        let msg = payload
            .get("error")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("agent returned HTTP {status}"));
        return Err(anyhow!("{msg}"));
    }

    let ok = payload.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let result = payload.get("result").cloned();
    let error = payload
        .get("error")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(MeshOpResult { ok, result, error })
}

// ---------------------------------------------------------------------------
// Helpers shared by local + remote paths.
// ---------------------------------------------------------------------------

/// Build the JSON envelope the helper expects: `{"id":…, "op":…, …extra}`.
fn build_envelope<P: Serialize>(op: &str, params: &P) -> Result<Value> {
    let mut v = serde_json::to_value(params).context("serializing params")?;
    if !v.is_object() {
        v = serde_json::json!({});
    }
    let map = v.as_object_mut().expect("guarded above");
    map.insert(
        "id".into(),
        Value::String(format!(
            "core-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )),
    );
    map.insert("op".into(), Value::String(op.to_string()));
    Ok(v)
}

fn parse_wire(line: &str) -> Result<MeshOpResult> {
    #[derive(serde::Deserialize)]
    struct Wire {
        ok: bool,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    }
    let w: Wire = serde_json::from_str(line.trim_end())
        .with_context(|| format!("parsing helper reply: {line:?}"))?;
    Ok(MeshOpResult {
        ok: w.ok,
        result: w.result,
        error: w.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_injects_id_and_op() {
        let v = build_envelope("install_wireguard", &serde_json::json!({})).unwrap();
        assert_eq!(v["op"], "install_wireguard");
        assert!(v["id"].as_str().unwrap().starts_with("core-"));
    }

    #[test]
    fn envelope_merges_extra_params() {
        let v = build_envelope("apply", &serde_json::json!({"rollback_after_sec": 60})).unwrap();
        assert_eq!(v["op"], "apply");
        assert_eq!(v["rollback_after_sec"], 60);
    }

    #[test]
    fn envelope_tolerates_non_object_params() {
        // Some callers pass `()` for unit ops; coerce to empty object so
        // the merge below doesn't panic.
        let v = build_envelope("commit", &serde_json::json!(null)).unwrap();
        assert_eq!(v["op"], "commit");
    }

    #[test]
    fn parse_wire_decodes_ok_response() {
        let r = parse_wire(r#"{"ok":true,"result":{"x":1}}"#).unwrap();
        assert!(r.ok);
        assert_eq!(r.result.unwrap()["x"], 1);
        assert!(r.error.is_none());
    }

    #[test]
    fn parse_wire_decodes_error_response() {
        let r = parse_wire(r#"{"ok":false,"error":"validate failed"}"#).unwrap();
        assert!(!r.ok);
        assert_eq!(r.error.as_deref(), Some("validate failed"));
    }
}
