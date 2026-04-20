use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateServerRequest {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: i64,
    pub ssh_user: Option<String>,
    pub ssh_port: Option<i64>,
}

fn default_port() -> i64 {
    3001
}

#[derive(Deserialize)]
pub struct HeartbeatRequest {
    pub agent_token: String,
    pub os_info: Option<String>,
    pub cpu_count: Option<i64>,
    pub memory_total: Option<i64>,
    pub docker_version: Option<String>,
}

/// GET /api/v1/servers
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, host, port, status, last_heartbeat, os_info, cpu_count, memory_total, docker_version, is_local, created_at, country, city, country_code
         FROM servers ORDER BY is_local DESC, created_at ASC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "host": row.get::<_, String>(2)?,
                "port": row.get::<_, i64>(3)?,
                "status": row.get::<_, String>(4)?,
                "last_heartbeat": row.get::<_, Option<String>>(5)?,
                "os_info": row.get::<_, Option<String>>(6)?,
                "cpu_count": row.get::<_, Option<i64>>(7)?,
                "memory_total": row.get::<_, Option<i64>>(8)?,
                "docker_version": row.get::<_, Option<String>>(9)?,
                "is_local": row.get::<_, bool>(10)?,
                "created_at": row.get::<_, String>(11)?,
                "country": row.get::<_, Option<String>>(12)?,
                "city": row.get::<_, Option<String>>(13)?,
                "country_code": row.get::<_, Option<String>>(14)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/servers
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateServerRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.host.trim().is_empty() {
        return Err(AppError::BadRequest("Name and host are required".into()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let agent_token = catalog::generate_password(32);

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO servers (id, name, host, port, agent_token, ssh_user, ssh_port)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.host.trim(),
            body.port,
            agent_token,
            body.ssh_user,
            body.ssh_port.unwrap_or(22)
        ],
    )?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "agent_token": agent_token
    })))
}

/// DELETE /api/v1/servers/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM servers WHERE id = ?1 AND is_local = 0", [&id])?;
    if rows == 0 {
        return Err(AppError::BadRequest(
            "Server not found or is local (cannot delete)".into(),
        ));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/servers/{id}/test
pub async fn test_connection(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host, port, agent_token FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Server {id} not found")))?
    };

    let url = format!("http://{}:{}/health", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Cannot connect to agent at {url}: {e}")))?;

    if resp.status().is_success() {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE servers SET status = 'online', last_heartbeat = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
            [&id],
        )?;
        Ok(Json(
            serde_json::json!({"ok": true, "message": "Agent is online"}),
        ))
    } else {
        Err(AppError::BadRequest(format!(
            "Agent responded with status: {}",
            resp.status()
        )))
    }
}

/// GET /api/v1/servers/{id}/metrics
pub async fn metrics(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host, port, agent_token FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Server {id} not found")))?
    };

    let url = format!("http://{}:{}/metrics", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Agent unreachable: {e}")))?;

    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON parse: {e}")))?;

    Ok(Json(data))
}

/// POST /api/v1/servers/heartbeat (public — uses token auth)
pub async fn heartbeat(
    State(state): State<SharedState>,
    Json(body): Json<HeartbeatRequest>,
) -> AppResult<impl IntoResponse> {
    // Read previous status first so we can detect an offline→online transition.
    let prev: Option<(String, String, String)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT id, name, status FROM servers WHERE agent_token = ?1",
            [&body.agent_token],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .ok()
    };

    let rows = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE servers SET status = 'online', last_heartbeat = datetime('now'),
             os_info = COALESCE(?2, os_info), cpu_count = COALESCE(?3, cpu_count),
             memory_total = COALESCE(?4, memory_total), docker_version = COALESCE(?5, docker_version),
             updated_at = datetime('now')
             WHERE agent_token = ?1",
            rusqlite::params![
                body.agent_token,
                body.os_info,
                body.cpu_count,
                body.memory_total,
                body.docker_version
            ],
        )?
    };

    if rows == 0 {
        return Err(AppError::Unauthorized);
    }

    // Fire reachable event on offline→online transition.
    if let Some((sid, name, prev_status)) = prev {
        if prev_status != "online" {
            let s = state.clone();
            tokio::spawn(async move {
                crate::alerts::hooks::fire_event(
                    &s,
                    "server_reachable",
                    None,
                    format!(
                        "Server {name} is back online (id: {sid}, previous status: {prev_status})"
                    ),
                )
                .await;
            });
        }
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// PUT /api/v1/servers/{id}/name — rename server.
pub async fn rename(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RenameServerRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name is required".into()));
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let rows = db.execute(
        "UPDATE servers SET name = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![name, id],
    )?;

    if rows == 0 {
        return Err(AppError::NotFound(format!("Server {id} not found")));
    }

    Ok(Json(serde_json::json!({"ok": true, "name": name})))
}

#[derive(Deserialize)]
pub struct RenameServerRequest {
    pub name: String,
}

/// GET /api/v1/servers/{id} — server detail with full metadata
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let server = db
        .query_row(
            "SELECT id, name, host, port, agent_token, status, last_heartbeat, os_info,
                cpu_count, memory_total, docker_version, is_local, created_at,
                country, city, country_code
         FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "host": row.get::<_, String>(2)?,
                    "port": row.get::<_, i64>(3)?,
                    "agent_token": row.get::<_, String>(4)?,
                    "status": row.get::<_, String>(5)?,
                    "last_heartbeat": row.get::<_, Option<String>>(6)?,
                    "os_info": row.get::<_, Option<String>>(7)?,
                    "cpu_count": row.get::<_, Option<i64>>(8)?,
                    "memory_total": row.get::<_, Option<i64>>(9)?,
                    "docker_version": row.get::<_, Option<String>>(10)?,
                    "is_local": row.get::<_, bool>(11)?,
                    "created_at": row.get::<_, String>(12)?,
                    "country": row.get::<_, Option<String>>(13)?,
                    "city": row.get::<_, Option<String>>(14)?,
                    "country_code": row.get::<_, Option<String>>(15)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Server {id} not found")))?;

    // Count services on this server
    let service_count: i64 = db.query_row(
        "SELECT COUNT(*) FROM services WHERE server_id = ?1 OR (?1 = 'local' AND (server_id IS NULL OR server_id = 'local'))",
        [&id],
        |row| row.get(0),
    ).unwrap_or(0);

    let mut result = server;
    result["service_count"] = serde_json::json!(service_count);
    Ok(Json(result))
}

/// GET /api/v1/servers/{id}/containers — proxy to agent: list containers
pub async fn containers(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token, is_local) = get_server_info(&state, &id)?;

    if is_local {
        // Return local containers directly
        let containers = state
            .docker
            .list_containers(Some(bollard::query_parameters::ListContainersOptions {
                all: true,
                ..Default::default()
            }))
            .await?;

        let items: Vec<serde_json::Value> = containers
            .iter()
            .map(|c| {
                serde_json::json!({
                    "id": c.id.as_deref().unwrap_or(""),
                    "names": c.names.as_ref().map(|n| n.join(", ")).unwrap_or_default(),
                    "image": c.image.as_deref().unwrap_or(""),
                    "state": format!("{:?}", c.state),
                    "status": c.status.as_deref().unwrap_or(""),
                })
            })
            .collect();
        return Ok(Json(serde_json::json!({"ok": true, "containers": items})));
    }

    // Proxy to remote agent
    let url = format!("http://{}:{}/api/v1/agent/status", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Agent unreachable: {e}")))?;
    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON: {e}")))?;
    Ok(Json(data))
}

/// POST /api/v1/servers/{id}/deploy — proxy deploy to remote agent
pub async fn deploy_to_server(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token, is_local) = get_server_info(&state, &id)?;

    if is_local {
        // Deploy locally
        let stack_name = body["stack_name"]
            .as_str()
            .ok_or_else(|| AppError::BadRequest("stack_name required".into()))?;
        let compose_yaml = body["compose_yaml"]
            .as_str()
            .ok_or_else(|| AppError::BadRequest("compose_yaml required".into()))?;
        let output =
            crate::docker::compose::deploy_stack(stack_name, compose_yaml, &state.config, None)
                .await
                .map_err(AppError::Internal)?;
        return Ok(Json(serde_json::json!({"ok": true, "output": output})));
    }

    // Proxy to remote agent
    let url = format!("http://{}:{}/api/v1/agent/deploy", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Agent unreachable: {e}")))?;
    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON: {e}")))?;
    Ok(Json(data))
}

/// POST /api/v1/servers/{id}/stop — proxy stop to remote agent
pub async fn stop_on_server(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token, is_local) = get_server_info(&state, &id)?;

    if is_local {
        let stack_name = body["stack_name"]
            .as_str()
            .ok_or_else(|| AppError::BadRequest("stack_name required".into()))?;
        let output = crate::docker::compose::down_stack(stack_name, &state.config)
            .await
            .map_err(AppError::Internal)?;
        return Ok(Json(serde_json::json!({"ok": true, "output": output})));
    }

    let url = format!("http://{}:{}/api/v1/agent/stop", host, port);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("Agent unreachable: {e}")))?;
    let data: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("JSON: {e}")))?;
    Ok(Json(data))
}

/// GET /api/v1/servers/install-script — generate agent install script
pub async fn install_script(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let token = params.get("token").cloned().unwrap_or_default();
    if token.is_empty() {
        return Err(AppError::BadRequest("token parameter required".into()));
    }

    // Get Pier server's public IP and port
    let server_ip = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT value FROM settings WHERE key = 'server.public_ip'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "YOUR_PIER_SERVER_IP".to_string())
    };
    let pier_port = state.config.port;

    let script = format!(
        r#"#!/bin/bash
set -e

# Pier Agent Installer
# Auto-generated by Pier

PIER_CORE_URL="http://{server_ip}:{pier_port}"
AGENT_TOKEN="{token}"
AGENT_PORT=3001

echo "=== Pier Agent Installer ==="
echo "Core server: $PIER_CORE_URL"

# 1. Install Docker if not present
if ! command -v docker &>/dev/null; then
    echo "Installing Docker..."
    curl -fsSL https://get.docker.com | sh
    systemctl enable --now docker
fi

# 2. Install Docker Compose plugin if not present
if ! docker compose version &>/dev/null; then
    echo "Installing Docker Compose plugin..."
    apt-get install -y docker-compose-plugin 2>/dev/null || true
fi

# 3. Download pier-agent binary
echo "Downloading pier-agent..."
mkdir -p /opt/pier/bin
curl -fsSL "$PIER_CORE_URL/api/v1/health" >/dev/null 2>&1 || echo "Warning: Cannot reach Pier core"

# Try to download from GitHub release
DOWNLOAD_URL="https://github.com/joveptesg/Pier/releases/download/latest/pier-agent-linux-amd64"
curl -fsSL -o /opt/pier/bin/pier-agent "$DOWNLOAD_URL" || {{
    echo "Error: Could not download pier-agent"
    echo "Please build from source: cargo build --release -p pier-agent"
    exit 1
}}
chmod +x /opt/pier/bin/pier-agent

# 4. Create systemd service
cat > /etc/systemd/system/pier-agent.service <<UNIT
[Unit]
Description=Pier Agent
After=network.target docker.service
Requires=docker.service

[Service]
Type=simple
Environment="PIER_AGENT_TOKEN=$AGENT_TOKEN"
Environment="PIER_AGENT_PORT=$AGENT_PORT"
Environment="PIER_AGENT_DATA_DIR=/var/lib/pier-agent"
Environment="RUST_LOG=info"
ExecStart=/opt/pier/bin/pier-agent
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
UNIT

# 5. Start agent
systemctl daemon-reload
systemctl enable --now pier-agent

echo ""
echo "=== Pier Agent installed ==="
echo "Agent port: $AGENT_PORT"
echo "Status: systemctl status pier-agent"
echo "Logs:   journalctl -u pier-agent -f"

# 6. Register with Pier core (send first heartbeat)
sleep 2
curl -s -X POST "$PIER_CORE_URL/api/v1/servers/heartbeat" \
    -H "Content-Type: application/json" \
    -d '{{"agent_token":"'"$AGENT_TOKEN"'","os_info":"'"$(uname -srm)"'","docker_version":"'"$(docker --version 2>/dev/null | cut -d' ' -f3 | tr -d ',')"'"}}'

echo ""
echo "Agent registered with Pier core."
"#
    );

    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/x-shellscript")],
        script,
    ))
}

/// Helper: extract server connection info
fn get_server_info(state: &SharedState, id: &str) -> Result<(String, i64, String, bool), AppError> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.query_row(
        "SELECT host, port, agent_token, is_local FROM servers WHERE id = ?1",
        [id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
            ))
        },
    )
    .map_err(|_| AppError::NotFound(format!("Server {id} not found")))
}
