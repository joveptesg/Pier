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
        .with_state(state);

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Pier Agent listening on {addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}
