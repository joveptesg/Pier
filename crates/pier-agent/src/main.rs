use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use bollard::Docker;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
mod helper_client;

mod shell;
mod tls;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct AgentState {
    token: String,
    docker: Docker,
    data_dir: String,
    /// Lowercase-hex SHA-256 of this agent's TLS leaf cert. Core pins this
    /// value; exposed read-only via `GET /api/v1/agent/tls/fingerprint` for
    /// diagnostics and re-pinning.
    tls_fingerprint: String,
    /// In-memory registry of running and recently-finished shell runs.
    /// Core polls these via `GET /api/v1/agent/shell/{run_id}` to drive the
    /// Tasks UI. Entries are GC'd a few minutes after `finished_at` (see
    /// `shell::start_gc_loop`).
    shell_runs: Arc<RwLock<HashMap<String, Arc<shell::RunHandle>>>>,
}

type SharedState = Arc<AgentState>;

// ---------------------------------------------------------------------------
// Auth middleware helper
// ---------------------------------------------------------------------------

pub(crate) fn verify_token(headers: &HeaderMap, state: &AgentState) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .map(|t| t == state.token)
        .unwrap_or(false)
}

macro_rules! require_auth {
    ($headers:expr, $state:expr) => {
        if !$crate::verify_token(&$headers, &$state) {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
        }
    };
}

pub(crate) use require_auth;

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

// ---------------------------------------------------------------------------
// GET /api/v1/agent/tls/fingerprint
//
// Returns the SHA-256 leaf fingerprint of the cert this agent serves. Core
// pins this during enrollment; the endpoint lets an operator (or a future
// re-pin flow) read it back. Authenticated — the fingerprint is not secret
// (it's in every TLS handshake), but keeping it behind the bearer avoids
// leaking which hosts run an agent.
// ---------------------------------------------------------------------------

async fn tls_fingerprint(
    State(state): State<SharedState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    require_auth!(headers, state);
    Json(serde_json::json!({ "fingerprint": state.tls_fingerprint })).into_response()
}

// ---------------------------------------------------------------------------
// GET /metrics
// ---------------------------------------------------------------------------

async fn metrics(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    require_auth!(headers, state);

    let mut sys = sysinfo::System::new_all();
    sys.refresh_all();

    let cpu_usage = sys.global_cpu_usage();
    let mem_total = sys.total_memory();
    let mem_used = sys.used_memory();
    let mem_pct = if mem_total > 0 {
        (mem_used as f64 / mem_total as f64) * 100.0
    } else {
        0.0
    };

    let disks: Vec<serde_json::Value> = sysinfo::Disks::new_with_refreshed_list()
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name().to_string_lossy(),
                "mount": d.mount_point().to_string_lossy(),
                "total": d.total_space(),
                "available": d.available_space(),
            })
        })
        .collect();

    let docker_info = match state.docker.version().await {
        Ok(v) => serde_json::json!({
            "version": v.version.unwrap_or_default(),
            "api_version": v.api_version.unwrap_or_default(),
            "os": v.os.unwrap_or_default(),
            "arch": v.arch.unwrap_or_default(),
        }),
        Err(_) => serde_json::json!(null),
    };

    let containers = match state.docker.list_containers(None).await {
        Ok(c) => c.len(),
        Err(_) => 0,
    };

    Json(serde_json::json!({
        "cpu_usage": format!("{cpu_usage:.1}"),
        "cpu_count": sys.cpus().len(),
        "memory_total": mem_total,
        "memory_used": mem_used,
        "memory_percent": format!("{mem_pct:.1}"),
        "hostname": sysinfo::System::host_name().unwrap_or_default(),
        "os": format!("{} {}", sysinfo::System::name().unwrap_or_default(), sysinfo::System::os_version().unwrap_or_default()),
        "uptime": sysinfo::System::uptime(),
        "disks": disks,
        "docker": docker_info,
        "containers": containers,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/v1/agent/deploy
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeployRequest {
    stack_name: String,
    compose_yaml: String,
}

async fn deploy(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<DeployRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let stack_dir = format!("{}/stacks/{}", state.data_dir, body.stack_name);
    if let Err(e) = tokio::fs::create_dir_all(&stack_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("mkdir: {e}")})),
        )
            .into_response();
    }

    let compose_path = format!("{stack_dir}/docker-compose.yml");
    if let Err(e) = tokio::fs::write(&compose_path, &body.compose_yaml).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("write: {e}")})),
        )
            .into_response();
    }

    let output = tokio::process::Command::new("docker")
        .args(["compose", "-f", &compose_path, "up", "-d"])
        .current_dir(&stack_dir)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => Json(serde_json::json!({
            "ok": true,
            "stdout": String::from_utf8_lossy(&out.stdout),
        }))
        .into_response(),
        Ok(out) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": String::from_utf8_lossy(&out.stderr),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("exec: {e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/agent/files — write (or delete) a file under the agent data dir.
//
// Used by the core to push this agent's Traefik config (static traefik.yml +
// per-service dynamic/*.yml). Paths are RELATIVE to PIER_AGENT_DATA_DIR and
// validated to stay within it (no absolute paths, no `..`), so this endpoint
// can never write outside the agent's data area.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct FileOpRequest {
    /// Path relative to the agent data dir, e.g. "traefik/dynamic/svc.yml".
    path: String,
    #[serde(default)]
    content: String,
    /// When true, delete the file instead of writing.
    #[serde(default)]
    delete: bool,
}

async fn write_file(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<FileOpRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let rel = std::path::Path::new(&body.path);
    if rel.is_absolute()
        || rel.components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "path must be relative and contain no `..`"})),
        )
            .into_response();
    }
    let target = std::path::Path::new(&state.data_dir).join(rel);

    if body.delete {
        let _ = tokio::fs::remove_file(&target).await;
        return Json(serde_json::json!({"ok": true, "deleted": true})).into_response();
    }

    if let Some(parent) = target.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": format!("mkdir: {e}")})),
            )
                .into_response();
        }
    }
    match tokio::fs::write(&target, &body.content).await {
        Ok(()) => Json(serde_json::json!({"ok": true, "path": body.path})).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("write: {e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/agent/stop
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StopRequest {
    stack_name: String,
    /// `docker compose down -v` — also remove the stack's named volumes.
    /// Used by the core on service DELETE; default off keeps "stop" semantics.
    #[serde(default)]
    delete_volumes: bool,
    /// After `down`, remove the stack directory `{data_dir}/stacks/{stack}`.
    /// Used on service DELETE so the agent doesn't accumulate orphan dirs.
    #[serde(default)]
    remove_dir: bool,
}

async fn stop(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<StopRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let stack_dir = format!("{}/stacks/{}", state.data_dir, body.stack_name);
    let compose_path = format!("{stack_dir}/docker-compose.yml");

    let mut args = vec!["compose", "-f", &compose_path, "down"];
    if body.delete_volumes {
        args.push("-v");
    }
    let output = tokio::process::Command::new("docker")
        .args(&args)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            if body.remove_dir {
                // Best-effort: the stack is gone; drop its compose dir too.
                let _ = tokio::fs::remove_dir_all(&stack_dir).await;
            }
            Json(serde_json::json!({"ok": true})).into_response()
        }
        Ok(out) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": String::from_utf8_lossy(&out.stderr),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/agent/exec
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ExecRequest {
    container_name: String,
    command: Vec<String>,
}

#[derive(Serialize)]
struct ExecResponse {
    ok: bool,
    exit_code: i32,
    stdout: String,
    stderr: String,
}

async fn exec_cmd(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<ExecRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let mut args = vec!["exec".to_string(), body.container_name];
    args.extend(body.command);

    let output = tokio::process::Command::new("docker")
        .args(&args)
        .output()
        .await;

    match output {
        Ok(out) => Json(ExecResponse {
            ok: out.status.success(),
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Mesh proxy — POST /api/v1/agent/mesh/{op}, GET /api/v1/agent/mesh/preflight
//
// The agent itself never speaks WireGuard. Core sends an op (`apply`,
// `generate_keypair`, …) to the agent; the agent forwards it down to
// pier-net-helper over /run/pier/net.sock and relays the helper's reply
// back over HTTPS. This keeps the privileged code path on the host short
// (helper → root, agent → unprivileged) and uniformly observable in
// Pier's HTTP logs.
//
// Preflight is the one read-only escape hatch: it does NOT round-trip
// through the helper, just checks whether the socket exists. Core uses
// it from the "Enable Mesh" wizard to tell the operator which nodes
// need an install-helper.sh retrofit.
// ---------------------------------------------------------------------------

#[cfg(unix)]
async fn mesh_proxy(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(op): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    // Empty body is acceptable for unit ops (commit/rollback/up/down/status).
    let extra = body.map(|j| j.0).unwrap_or_else(|| serde_json::json!({}));

    // Random id so concurrent core requests don't collide in helper logs.
    // We don't depend on a uuid crate just for this — a 64-bit counter
    // mixed with the request timestamp is enough.
    let id = format!(
        "req-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        rand::random::<u32>()
    );

    let body = match helper_client::build_request(&id, &op, &extra) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("bad request: {e:#}")})),
            )
                .into_response();
        }
    };

    match helper_client::call(&body).await {
        Ok(resp) if resp.ok => Json(serde_json::json!({
            "ok": true,
            "result": resp.result.unwrap_or(serde_json::json!({})),
        }))
        .into_response(),
        Ok(resp) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": resp.error.unwrap_or_else(|| "helper returned ok=false with no error".into()),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("helper unreachable: {e:#}"),
            })),
        )
            .into_response(),
    }
}

#[cfg(not(unix))]
async fn mesh_proxy(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(_op): Path<String>,
    _body: Option<Json<serde_json::Value>>,
) -> impl IntoResponse {
    require_auth!(headers, state);
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "ok": false,
            "error": "mesh is Linux-only; this agent is built for a non-unix target",
        })),
    )
        .into_response()
}

async fn mesh_preflight(State(state): State<SharedState>, headers: HeaderMap) -> impl IntoResponse {
    require_auth!(headers, state);
    let socket = std::env::var("PIER_NET_HELPER_SOCKET")
        .unwrap_or_else(|_| "/run/pier/net.sock".to_string());
    let helper_available = std::path::Path::new(&socket).exists();
    Json(serde_json::json!({
        "helper_available": helper_available,
        "socket_path": socket,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// POST /api/v1/agent/auth/rotate
//
// Core calls this with the CURRENT bearer to swap in a fresh
// long-term token. We overwrite /etc/pier-agent/auth.env (which the
// install script seeded and the systemd unit picks up via
// EnvironmentFile=) and then exit so systemd's Restart=always
// respawns us with the new PIER_AGENT_TOKEN in our environment.
//
// Why exit instead of hot-swapping `state.token`?
//   * `state.token` is held by every in-flight request as `&AgentState`.
//     A mid-request swap would create a race where the SAME request
//     could see both the old and the new value depending on when it
//     reads, and the bearer string is compared via `==`, not a guarded
//     accessor. Restarting closes every connection cleanly.
//   * systemd respawn is a well-understood failure mode the operator
//     can already monitor; adding bespoke runtime mutability buys us
//     nothing over it.
//
// Race window: between writing the env file and exit, requests with
// the OLD bearer still authenticate. We delay the exit by ~500ms so
// the rotation response itself reaches core before the process dies;
// if we exited immediately the response might fail at the TCP layer
// even though the rotation succeeded.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RotateRequest {
    new_token: String,
}

async fn auth_rotate(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<RotateRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let new_token = body.new_token.trim();
    if new_token.is_empty() || !new_token.starts_with("pier_srv_") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "ok": false,
                "error": "new_token must be a non-empty `pier_srv_…` value",
            })),
        )
            .into_response();
    }

    // Atomic write: dump to a sibling file with 0600, then rename. We
    // can't just append-and-truncate because a crash between truncate
    // and write would leave the agent with no token on next boot.
    let path = "/etc/pier-agent/auth.env";
    let tmp = "/etc/pier-agent/auth.env.new";
    let body = format!("PIER_AGENT_TOKEN={new_token}\n");

    if let Err(e) = tokio::fs::write(tmp, &body).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("write {tmp}: {e}"),
            })),
        )
            .into_response();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            tokio::fs::set_permissions(tmp, std::fs::Permissions::from_mode(0o600)).await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("chmod {tmp}: {e}"),
                })),
            )
                .into_response();
        }
    }
    if let Err(e) = tokio::fs::rename(tmp, path).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "ok": false,
                "error": format!("rename {tmp} → {path}: {e}"),
            })),
        )
            .into_response();
    }

    // Schedule the respawn just after we return the response. If we
    // exit before the HTTP write finishes, core sees a dropped
    // connection even though the rotation succeeded.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("auth rotated — exiting for systemd to respawn with new token");
        std::process::exit(0);
    });

    Json(serde_json::json!({"ok": true})).into_response()
}

// ---------------------------------------------------------------------------
// GET /api/v1/agent/status
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StatusQuery {
    stack_name: String,
}

async fn stack_status(
    State(state): State<SharedState>,
    headers: HeaderMap,
    axum::extract::Query(q): axum::extract::Query<StatusQuery>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let compose_path = format!(
        "{}/stacks/{}/docker-compose.yml",
        state.data_dir, q.stack_name
    );

    let output = tokio::process::Command::new("docker")
        .args(["compose", "-f", &compose_path, "ps", "--format", "json"])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let raw = String::from_utf8_lossy(&out.stdout);
            // docker compose ps --format json outputs one JSON per line
            let containers: Vec<serde_json::Value> = raw
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            Json(serde_json::json!({"ok": true, "containers": containers})).into_response()
        }
        Ok(out) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": String::from_utf8_lossy(&out.stderr),
            })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("{e}")})),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// POST /api/v1/agent/promote
//
// Promotes this agent to a full pier-core. The Core passes in a promotion bundle
// (exported from its database) plus optional parameters. The agent:
//   1. Writes the bundle to its data dir.
//   2. Writes a shell script that downloads pier-core, imports the bundle,
//      installs a systemd unit, stops pier-agent and starts pier.
//   3. Spawns the script detached from the HTTP request and returns 202.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct PromoteRequest {
    /// Opaque bundle produced by Core's `GET /api/v1/servers/{id}/promote-bundle`.
    /// We don't validate its structure here — pier-core's `--import-bundle`
    /// CLI does that and fails loudly if the bundle is malformed.
    bundle: serde_json::Value,
    /// Download URL for the pier-core binary. Defaults to GitHub latest release.
    #[serde(default)]
    core_download_url: Option<String>,
    /// Port pier-core should listen on after promotion (default: 8443).
    #[serde(default)]
    core_port: Option<u16>,
}

async fn promote(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<PromoteRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    // 1. Write bundle to disk.
    let promote_dir = format!("{}/promote", state.data_dir);
    if let Err(e) = tokio::fs::create_dir_all(&promote_dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("mkdir: {e}")})),
        )
            .into_response();
    }
    let bundle_path = format!("{promote_dir}/bundle.json");
    let bundle_text = match serde_json::to_string(&body.bundle) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("bundle serialize: {e}")})),
            )
                .into_response();
        }
    };
    if let Err(e) = tokio::fs::write(&bundle_path, &bundle_text).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("write bundle: {e}")})),
        )
            .into_response();
    }

    // 2. Build and write the promotion shell script.
    let download_url = body.core_download_url.unwrap_or_else(|| {
        "https://github.com/joveptesg/pier/releases/download/latest/pier-linux-amd64".to_string()
    });
    let core_port = body.core_port.unwrap_or(8443);
    let script_path = format!("{promote_dir}/promote.sh");
    let script = promotion_script(&download_url, &bundle_path, core_port);

    if let Err(e) = tokio::fs::write(&script_path, &script).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("write script: {e}")})),
        )
            .into_response();
    }
    // Make the script executable on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ =
            tokio::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).await;
    }

    // 3. Spawn detached. `setsid` detaches from the controlling terminal, and the
    //    final `&` + redirect to a log file ensures the script survives after
    //    pier-agent is killed by its own script.
    let log_path = format!("{promote_dir}/promote.log");
    let spawn_cmd = format!(
        "setsid nohup bash {script_path} >{log_path} 2>&1 </dev/null &",
        script_path = script_path,
        log_path = log_path
    );
    let spawn = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&spawn_cmd)
        .spawn();
    match spawn {
        Ok(_) => {
            tracing::info!("Promotion started; see {log_path} on the target server for progress");
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({
                    "ok": true,
                    "message": "Promotion started. Agent will stop itself and pier-core will take over.",
                    "log_path": log_path,
                    "bundle_path": bundle_path,
                    "script_path": script_path,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("spawn: {e}")})),
        )
            .into_response(),
    }
}

/// Build the self-contained promotion shell script. No interpolation beyond
/// the inputs — keep it portable and debuggable.
fn promotion_script(download_url: &str, bundle_path: &str, core_port: u16) -> String {
    format!(
        r#"#!/bin/bash
set -e
LOG() {{ echo "[$(date '+%F %T')] $*"; }}

LOG "=== pier-agent → pier-core promotion ==="

CORE_URL="{download_url}"
BUNDLE="{bundle_path}"
CORE_DATA_DIR="/var/lib/pier"
CORE_PORT="{core_port}"

mkdir -p /opt/pier/bin "$CORE_DATA_DIR"
LOG "Downloading pier-core from $CORE_URL"
if ! curl -fsSL -o /opt/pier/bin/pier "$CORE_URL"; then
    LOG "ERROR: download failed"
    exit 1
fi
chmod +x /opt/pier/bin/pier

LOG "Importing bundle into fresh database"
PIER_DATA_DIR="$CORE_DATA_DIR" /opt/pier/bin/pier --import-bundle "$BUNDLE"

LOG "Writing systemd unit for pier-core"
cat >/etc/systemd/system/pier.service <<'UNIT'
[Unit]
Description=Pier
After=network.target docker.service
Requires=docker.service

[Service]
Type=simple
Environment="PIER_DATA_DIR=/var/lib/pier"
Environment="PIER_PORT={core_port}"
Environment="RUST_LOG=info"
ExecStart=/opt/pier/bin/pier
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable pier

LOG "Stopping pier-agent"
systemctl stop pier-agent || true

LOG "Starting pier-core on port $CORE_PORT"
systemctl start pier

LOG "Promotion complete. Visit http://$(hostname -I | awk '{{print $1}}'):$CORE_PORT to create an admin user."
"#
    )
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Install the process-wide rustls crypto provider before any TLS use.
    // `.ok()` because a second install (e.g. in tests) is harmless.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let token = std::env::var("PIER_AGENT_TOKEN").unwrap_or_else(|_| {
        tracing::warn!("PIER_AGENT_TOKEN not set — using empty token (insecure!)");
        String::new()
    });

    let port: u16 = std::env::var("PIER_AGENT_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3001);

    let data_dir =
        std::env::var("PIER_AGENT_DATA_DIR").unwrap_or_else(|_| "/var/lib/pier-agent".into());
    tokio::fs::create_dir_all(&data_dir).await?;

    // TLS: load (or generate) the agent's self-signed cert. The installer
    // normally pre-generates it via openssl so the leaf fingerprint is known
    // to core at handshake time; we generate one here only as a fallback.
    let tls_dir =
        std::env::var("PIER_AGENT_TLS_DIR").unwrap_or_else(|_| "/etc/pier-agent/tls".into());
    let agent_tls = tls::load_or_generate(std::path::Path::new(&tls_dir)).await?;
    tracing::info!("Agent TLS leaf fingerprint: {}", agent_tls.fingerprint);

    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| anyhow::anyhow!("Docker connect failed: {e}"))?;

    let state = Arc::new(AgentState {
        token,
        docker,
        data_dir,
        tls_fingerprint: agent_tls.fingerprint.clone(),
        shell_runs: Arc::new(RwLock::new(HashMap::new())),
    });

    // Periodically drop finished shell runs from the in-memory registry.
    // Core has already persisted them by then; keeping a 5-minute grace
    // window covers a transient core restart that hasn't pulled the final
    // snapshot yet.
    shell::start_gc_loop(state.clone());

    let app = Router::new()
        // Public health endpoint
        .route("/health", get(health))
        // Authenticated endpoints
        .route("/metrics", get(metrics))
        .route("/api/v1/agent/tls/fingerprint", get(tls_fingerprint))
        .route("/api/v1/agent/deploy", post(deploy))
        .route("/api/v1/agent/files", post(write_file))
        .route("/api/v1/agent/stop", post(stop))
        .route("/api/v1/agent/exec", post(exec_cmd))
        .route("/api/v1/agent/status", get(stack_status))
        .route("/api/v1/agent/promote", post(promote))
        // Mesh: thin proxy into pier-net-helper. {op} is the helper op
        // name (install_wireguard, apply, …). Preflight is a read-only
        // check that doesn't round-trip through the helper.
        .route("/api/v1/agent/mesh/preflight", get(mesh_preflight))
        .route("/api/v1/agent/mesh/{op}", post(mesh_proxy))
        // Token rotation. Core posts the new long-term token; the
        // agent rewrites /etc/pier-agent/auth.env and exits so systemd
        // respawns it with the new env var. Auth header is the OLD
        // token — once we exit, that token is invalidated on core too.
        .route("/api/v1/agent/auth/rotate", post(auth_rotate))
        // Ad-hoc shell runner. Core POSTs a command and polls /shell/{id}
        // until status is terminal. No WS — keeps the agent's dependency
        // surface small and the protocol HTTP-cacheable.
        .route("/api/v1/agent/shell", post(shell::start_run))
        .route("/api/v1/agent/shell/{run_id}", get(shell::get_run))
        .route(
            "/api/v1/agent/shell/{run_id}/cancel",
            post(shell::cancel_run),
        )
        .with_state(state);

    // `[::]:PORT` binds both IPv4 and IPv6 on Linux by default
    // (IPV6_V6ONLY=0). Without this, an IPv6-only host running this
    // agent can't accept core's API calls — they'd never reach the
    // listener. Operators who need v4-only can override with
    // PIER_AGENT_BIND=0.0.0.0 below.
    let bind_host = std::env::var("PIER_AGENT_BIND").unwrap_or_else(|_| "[::]".to_string());
    let addr = format!("{bind_host}:{port}");
    let sock_addr: std::net::SocketAddr = addr
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {addr:?}: {e}"))?;
    tracing::info!("Pier Agent listening on https://{addr}");

    // HTTPS only: core pins the leaf fingerprint, so the channel is both
    // encrypted and authenticated against cert swaps.
    axum_server::bind_rustls(sock_addr, agent_tls.config)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}
