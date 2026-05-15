//! Thin client for `pier-net-helper` over `/run/pier/net.sock`.
//!
//! Wire protocol: line-delimited JSON, one request per connection. The
//! helper closes the socket after a single response, so we don't need to
//! multiplex — every call opens a fresh connection.
//!
//! The agent itself never talks to `wg` / `wg-quick` / `apt` directly. It
//! merely forwards calls from `pier-core` (which authenticates as the
//! agent's controller) into the helper, which has the privileges to act.
//! This file is the tiny seam between the two.

#![cfg(unix)]

use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Default socket path. Matches the helper's `DEFAULT_SOCKET_PATH` and
/// the unit file's `RuntimeDirectory=pier`.
const DEFAULT_SOCKET_PATH: &str = "/run/pier/net.sock";

/// Hard ceiling on how long a single helper round-trip can take. The
/// slowest legitimate op is `install_wireguard` (an apt install over a
/// slow link). Anything longer than this means the helper is either
/// stuck or the host is melting — either way, surface a timeout to core
/// rather than blocking the agent forever.
const HELPER_TIMEOUT: Duration = Duration::from_secs(120);

/// Outcome of one helper round-trip. `id` is the caller's request id
/// echoed back; we keep the same id field so multi-step orchestration
/// in core can correlate logs across nodes.
pub struct HelperResponse {
    pub ok: bool,
    pub result: Option<Value>,
    pub error: Option<String>,
}

fn socket_path() -> PathBuf {
    std::env::var("PIER_NET_HELPER_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_PATH))
}

/// Send a JSON value to the helper and read its single-line JSON reply.
///
/// `body` is the full request envelope including `id` and `op`. Callers
/// build it via [`build_request`] to keep field naming consistent with
/// `pier-net-helper::protocol::Request`.
pub async fn call(body: &Value) -> Result<HelperResponse> {
    let path = socket_path();
    let stream = tokio::time::timeout(Duration::from_secs(2), UnixStream::connect(&path))
        .await
        .with_context(|| format!("connecting to {}", path.display()))?
        .with_context(|| format!("connecting to {}", path.display()))?;

    let (read_half, mut write_half) = stream.into_split();
    let mut bytes = serde_json::to_vec(body)?;
    bytes.push(b'\n');

    tokio::time::timeout(HELPER_TIMEOUT, async move {
        write_half.write_all(&bytes).await?;
        write_half.shutdown().await?;
        anyhow::Ok(())
    })
    .await
    .map_err(|_| anyhow!("helper write timed out after {:?}", HELPER_TIMEOUT))??;

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    tokio::time::timeout(HELPER_TIMEOUT, reader.read_line(&mut line))
        .await
        .map_err(|_| anyhow!("helper read timed out after {:?}", HELPER_TIMEOUT))?
        .context("reading helper response")?;

    // The helper's Response shape; we don't pull the `pier-net-helper`
    // crate as a dependency here to keep the agent build lightweight.
    #[derive(serde::Deserialize)]
    struct Wire {
        ok: bool,
        #[serde(default)]
        result: Option<Value>,
        #[serde(default)]
        error: Option<String>,
    }
    let wire: Wire = serde_json::from_str(line.trim_end())
        .with_context(|| format!("parsing helper reply: {line:?}"))?;
    Ok(HelperResponse {
        ok: wire.ok,
        result: wire.result,
        error: wire.error,
    })
}

/// Build a request envelope. The helper accepts an internally-tagged
/// shape: `{"id":"…","op":"…", <extra fields siblings to op>}`. Callers
/// can pass an empty `extra` object for unit ops.
pub fn build_request<E: Serialize>(id: &str, op: &str, extra: &E) -> Result<Value> {
    let mut v = serde_json::to_value(extra).context("serializing helper params")?;
    if !v.is_object() {
        // Allow `()` / `null` for unit ops — coerce to an empty object so
        // the merge below has something to plug `id` and `op` into.
        v = serde_json::json!({});
    }
    let map = v.as_object_mut().expect("guarded above");
    map.insert("id".into(), Value::String(id.to_string()));
    map.insert("op".into(), Value::String(op.to_string()));
    Ok(v)
}
