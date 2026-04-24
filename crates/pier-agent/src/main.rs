use anyhow::Result;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use bollard::Docker;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct AgentState {
    token: String,
    docker: Docker,
    data_dir: String,
}

type SharedState = Arc<AgentState>;

// ---------------------------------------------------------------------------
// Auth middleware helper
// ---------------------------------------------------------------------------

fn verify_token(headers: &HeaderMap, state: &AgentState) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.strip_prefix("Bearer ").unwrap_or(v))
        .map(|t| t == state.token)
        .unwrap_or(false)
}

macro_rules! require_auth {
    ($headers:expr, $state:expr) => {
        if !verify_token(&$headers, &$state) {
            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({"error": "unauthorized"}))).into_response();
        }
    };
}

// ---------------------------------------------------------------------------
// GET /health
// ---------------------------------------------------------------------------

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
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
// POST /api/v1/agent/stop
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct StopRequest {
    stack_name: String,
}

async fn stop(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<StopRequest>,
) -> impl IntoResponse {
    require_auth!(headers, state);

    let compose_path = format!(
        "{}/stacks/{}/docker-compose.yml",
        state.data_dir, body.stack_name
    );

    let output = tokio::process::Command::new("docker")
        .args(["compose", "-f", &compose_path, "down"])
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => Json(serde_json::json!({"ok": true})).into_response(),
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
        "https://github.com/joveptesg/Pier/releases/download/latest/pier-linux-amd64".to_string()
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

    let docker = Docker::connect_with_local_defaults()
        .map_err(|e| anyhow::anyhow!("Docker connect failed: {e}"))?;

    let state = Arc::new(AgentState {
        token,
        docker,
        data_dir,
    });

    let app = Router::new()
        // Public health endpoint
        .route("/health", get(health))
        // Authenticated endpoints
        .route("/metrics", get(metrics))
        .route("/api/v1/agent/deploy", post(deploy))
        .route("/api/v1/agent/stop", post(stop))
        .route("/api/v1/agent/exec", post(exec_cmd))
        .route("/api/v1/agent/status", get(stack_status))
        .route("/api/v1/agent/promote", post(promote))
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Pier Agent listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
