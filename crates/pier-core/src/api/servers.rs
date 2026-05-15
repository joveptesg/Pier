use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::PEER_TOKEN_HEADER;
use crate::auth::server_token;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// What kind of remote infrastructure a `servers` row represents.
/// Stored in the `servers.kind` column (migration 31).
///
/// * `local` — this very machine (`is_local = 1`).
/// * `agent` — remote host running `pier-agent` (lightweight, stateless).
///   `host`, `port`, `agent_token` are the connection params.
/// * `peer`  — remote host running a full `pier-core` with its own DB.
///   `url` is the HTTPS base address, `agent_token` reused as peer grant token.
pub const KIND_LOCAL: &str = "local";
pub const KIND_AGENT: &str = "agent";
pub const KIND_PEER: &str = "peer";

#[derive(Deserialize)]
pub struct CreateServerRequest {
    pub name: String,
    /// "agent" (default) or "peer". `local` is set internally, never from request.
    #[serde(default = "default_kind")]
    pub kind: String,

    // Agent fields (required when kind == "agent").
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default = "default_port")]
    pub port: i64,
    pub ssh_user: Option<String>,
    pub ssh_port: Option<i64>,

    // Peer fields (required when kind == "peer").
    #[serde(default)]
    pub url: Option<String>,
    /// Token issued by the remote core ("Allow external control" → Issue Token).
    #[serde(default)]
    pub api_token: Option<String>,
}

fn default_kind() -> String {
    KIND_AGENT.to_string()
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
        "SELECT id, name, kind, host, port, url, status, last_heartbeat, os_info, cpu_count,
                memory_total, docker_version, remote_version, last_error, is_local, created_at,
                country, city, country_code
         FROM servers ORDER BY is_local DESC, kind ASC, created_at ASC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "kind": row.get::<_, String>(2)?,
                "host": row.get::<_, String>(3)?,
                "port": row.get::<_, i64>(4)?,
                "url": row.get::<_, Option<String>>(5)?,
                "status": row.get::<_, String>(6)?,
                "last_heartbeat": row.get::<_, Option<String>>(7)?,
                "os_info": row.get::<_, Option<String>>(8)?,
                "cpu_count": row.get::<_, Option<i64>>(9)?,
                "memory_total": row.get::<_, Option<i64>>(10)?,
                "docker_version": row.get::<_, Option<String>>(11)?,
                "remote_version": row.get::<_, Option<String>>(12)?,
                "last_error": row.get::<_, Option<String>>(13)?,
                "is_local": row.get::<_, bool>(14)?,
                "created_at": row.get::<_, String>(15)?,
                "country": row.get::<_, Option<String>>(16)?,
                "city": row.get::<_, Option<String>>(17)?,
                "country_code": row.get::<_, Option<String>>(18)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/servers
/// Two shapes accepted:
///   { kind: "agent", name, host, port? }              — Core generates agent_token.
///   { kind: "peer",  name, url,  api_token }          — Core stores caller-provided token.
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateServerRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name is required".into()));
    }
    let id = uuid::Uuid::new_v4().to_string();

    match body.kind.as_str() {
        KIND_AGENT => {
            let host = body
                .host
                .as_deref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::BadRequest("host is required for agent".into()))?
                .to_string();
            // Issue a short-lived bootstrap token. The long-term agent_token is
            // minted by /handshake on first contact from the agent and is the
            // only credential that ever leaves the install command.
            //
            // `agent_token` (plaintext column, NOT NULL since migration 5)
            // stays empty until handshake — the row is identifiable by
            // `bootstrap_token_hash`, not by any callable credential.
            let bootstrap = server_token::generate_bootstrap();
            let now = chrono::Utc::now().timestamp();
            let expires_at = now + server_token::BOOTSTRAP_TTL_SECS;
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "INSERT INTO servers
                        (id, name, kind, host, port, agent_token,
                         bootstrap_token_hash, bootstrap_expires_at,
                         ssh_user, ssh_port)
                     VALUES (?1, ?2, 'agent', ?3, ?4, '',
                             ?5, ?6,
                             ?7, ?8)",
                    rusqlite::params![
                        id,
                        name,
                        host,
                        body.port,
                        server_token::hash(&bootstrap.plaintext),
                        expires_at,
                        body.ssh_user,
                        body.ssh_port.unwrap_or(22)
                    ],
                )?;
            }
            Ok(Json(serde_json::json!({
                "ok": true,
                "id": id,
                "kind": "agent",
                "bootstrap_token": bootstrap.plaintext,
                "bootstrap_expires_at": expires_at,
            })))
        }
        KIND_PEER => {
            let url = body
                .url
                .as_deref()
                .map(|s| s.trim().trim_end_matches('/'))
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::BadRequest("url is required for peer".into()))?
                .to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(AppError::BadRequest(
                    "url must start with http:// or https://".into(),
                ));
            }
            let token = body
                .api_token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AppError::BadRequest("api_token is required for peer".into()))?
                .to_string();
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "INSERT INTO servers (id, name, kind, host, port, url, agent_token)
                     VALUES (?1, ?2, 'peer', '', 0, ?3, ?4)",
                    rusqlite::params![id, name, url, token],
                )?;
            }
            // Synchronous probe so the UI gets a meaningful status immediately.
            let _ = probe_peer(&state, &id).await;
            Ok(Json(serde_json::json!({
                "ok": true,
                "id": id,
                "kind": "peer",
            })))
        }
        other => Err(AppError::BadRequest(format!(
            "unknown kind '{other}' — expected 'agent' or 'peer'"
        ))),
    }
}

#[derive(Deserialize)]
pub struct HandshakeRequest {
    /// `pier_boot_…` plaintext from the install command.
    pub bootstrap_token: String,
    /// Best-effort host facts so the operator sees a populated server card
    /// without waiting for the first heartbeat round-trip.
    pub os_info: Option<String>,
    pub docker_version: Option<String>,
}

/// POST /api/v1/servers/{id}/handshake (public — bootstrap-token auth)
///
/// Spends a one-shot bootstrap token and mints the long-term agent credential.
/// The plaintext long-term token is returned **exactly once** in the response
/// and persisted only as sha256 on core; the agent stores it in its systemd
/// `Environment=` file.
///
/// Idempotency: a row with an already-redeemed bootstrap (NULL hash) responds
/// 401 — the operator must recreate the server in the UI to get a fresh
/// bootstrap. This is intentional: a "second handshake" almost always means
/// the install command leaked or was re-run accidentally, and silently
/// rotating the token would mask that.
pub async fn handshake(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<HandshakeRequest>,
) -> AppResult<impl IntoResponse> {
    let bootstrap = body.bootstrap_token.trim();
    if bootstrap.is_empty() {
        return Err(AppError::BadRequest("bootstrap_token required".into()));
    }

    let now = chrono::Utc::now().timestamp();
    let bootstrap_hash = server_token::hash(bootstrap);

    // Look up the row by hash + id together. Matching on `id` as well stops a
    // valid bootstrap from being redeemed against the wrong server row in the
    // (unlikely) event of a hash collision or operator copy-paste error.
    let row: Option<(Option<String>, Option<i64>)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT bootstrap_token_hash, bootstrap_expires_at
             FROM servers
             WHERE id = ?1 AND kind = 'agent'",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                ))
            },
        )
        .ok()
    };

    let Some((stored_hash, expires_at)) = row else {
        return Err(AppError::Unauthorized);
    };

    // Already redeemed (or never had a bootstrap — legacy row).
    let Some(stored_hash) = stored_hash else {
        return Err(AppError::Unauthorized);
    };

    if stored_hash != bootstrap_hash {
        return Err(AppError::Unauthorized);
    }
    if !server_token::bootstrap_alive(expires_at, now) {
        return Err(AppError::Unauthorized);
    }

    // Mint long-term credential. From this point on the agent authenticates
    // with `agent_token` plaintext, and core looks it up via `agent_token_hash`.
    let agent = server_token::generate_agent();
    let agent_hash = server_token::hash(&agent.plaintext);

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let updated = db.execute(
            "UPDATE servers
             SET agent_token = ?1,
                 agent_token_hash = ?2,
                 agent_token_prefix = ?3,
                 bootstrap_token_hash = NULL,
                 bootstrap_expires_at = NULL,
                 status = 'online',
                 last_heartbeat = datetime('now'),
                 os_info = COALESCE(?4, os_info),
                 docker_version = COALESCE(?5, docker_version),
                 last_error = NULL,
                 updated_at = datetime('now')
             WHERE id = ?6
               AND kind = 'agent'
               AND bootstrap_token_hash = ?7",
            rusqlite::params![
                agent.plaintext,
                agent_hash,
                agent.prefix,
                body.os_info,
                body.docker_version,
                id,
                bootstrap_hash,
            ],
        )?;
        // Race: another concurrent handshake redeemed first. Treat as
        // unauthorized — only one bootstrap → one long-term token.
        if updated == 0 {
            return Err(AppError::Unauthorized);
        }
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "agent_token": agent.plaintext,
        "agent_token_prefix": agent.prefix,
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

/// POST /api/v1/servers/{id}/test — checks connectivity to the right endpoint for the kind.
pub async fn test_connection(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let kind = get_server_kind(&state, &id)?;
    match kind.as_str() {
        KIND_PEER => {
            let info = probe_peer(&state, &id).await?;
            Ok(Json(
                serde_json::json!({"ok": true, "kind": "peer", "peer": info}),
            ))
        }
        KIND_AGENT => {
            let (host, port, agent_token, _, _) = get_server_info(&state, &id)?;
            let url = format!("http://{host}:{port}/health");
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {agent_token}"))
                .send()
                .await
                .map_err(|e| {
                    AppError::BadRequest(format!("Cannot connect to agent at {url}: {e}"))
                })?;
            if resp.status().is_success() {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "UPDATE servers SET status = 'online', last_heartbeat = datetime('now'),
                        last_error = NULL, updated_at = datetime('now') WHERE id = ?1",
                    [&id],
                )?;
                Ok(Json(
                    serde_json::json!({"ok": true, "kind": "agent", "message": "Agent is online"}),
                ))
            } else {
                Err(AppError::BadRequest(format!(
                    "Agent responded with status: {}",
                    resp.status()
                )))
            }
        }
        KIND_LOCAL => Ok(Json(
            serde_json::json!({"ok": true, "kind": "local", "message": "Local core"}),
        )),
        other => Err(AppError::BadRequest(format!("unknown kind '{other}'"))),
    }
}

/// Probe a peer core's `/api/v1/peers/probe` endpoint with its X-Pier-Peer-Token.
/// Updates `servers` row (status, last_heartbeat, remote_version, last_error).
pub(crate) async fn probe_peer(state: &SharedState, id: &str) -> AppResult<serde_json::Value> {
    let (url, token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT url, agent_token FROM servers WHERE id = ?1 AND kind = 'peer'",
            [id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, String>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Peer {id} not found")))?
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
    let probe_url = format!("{}/api/v1/peers/probe", normalize_peer_url(&url));
    let resp = client
        .get(&probe_url)
        .header(PEER_TOKEN_HEADER, &token)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await.unwrap_or(serde_json::json!({}));
            let version = body
                .get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            db.execute(
                "UPDATE servers SET status = 'online', last_heartbeat = datetime('now'),
                    remote_version = COALESCE(?2, remote_version), last_error = NULL,
                    updated_at = datetime('now') WHERE id = ?1",
                rusqlite::params![id, version],
            )?;
            Ok(body)
        }
        Ok(r) => {
            let status = r.status();
            let text = r.text().await.unwrap_or_default();
            let err = format!("HTTP {status}: {text}");
            mark_peer_error(state, id, &err)?;
            Err(AppError::BadRequest(err))
        }
        Err(e) => {
            let err = format!("unreachable: {e}");
            mark_peer_error(state, id, &err)?;
            Err(AppError::BadRequest(err))
        }
    }
}

/// Normalise a stored peer URL so legacy `http://...` rows still work after
/// the admin panel went TLS-only. Idempotent — `https://` and other schemes
/// pass through unchanged. Operators don't need to hand-edit the `servers`
/// table after upgrade.
fn normalize_peer_url(url: &str) -> String {
    match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    }
}

fn mark_peer_error(state: &SharedState, id: &str, msg: &str) -> AppResult<()> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "UPDATE servers SET status = 'offline', last_error = ?2, updated_at = datetime('now')
         WHERE id = ?1",
        rusqlite::params![id, msg],
    )?;
    Ok(())
}

/// Proxy any request to a peer's API. Route: `/api/v1/servers/{id}/proxy/{*rest}`.
/// Rejects the call if the target server is not `kind='peer'`.
pub async fn proxy(
    State(state): State<SharedState>,
    Path((id, rest)): Path<(String, String)>,
    req: Request,
) -> Result<Response, AppError> {
    let (url, token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT url, agent_token FROM servers WHERE id = ?1 AND kind = 'peer'",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    row.get::<_, String>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Peer {id} not found")))?
    };

    let method = req.method().clone();
    let query = req.uri().query().map(|s| s.to_string());
    let mut target = format!("{}/api/v1/{rest}", normalize_peer_url(&url));
    if let Some(q) = query {
        target.push('?');
        target.push_str(&q);
    }

    let mut fwd_headers = HeaderMap::new();
    for (name, value) in req.headers().iter() {
        if should_forward_header(name) {
            fwd_headers.insert(name.clone(), value.clone());
        }
    }
    fwd_headers.insert(
        HeaderName::from_static("x-pier-peer-token"),
        HeaderValue::from_str(&token)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("header: {e}")))?,
    );

    let body_bytes = axum::body::to_bytes(req.into_body(), 32 * 1024 * 1024)
        .await
        .map_err(|e| AppError::BadRequest(format!("body read: {e}")))?;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;
    let reqwest_method = reqwest::Method::from_bytes(method.as_str().as_bytes())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("method: {e}")))?;
    let resp = client
        .request(reqwest_method, &target)
        .headers(
            fwd_headers
                .iter()
                .map(|(n, v)| {
                    (
                        reqwest::header::HeaderName::from_bytes(n.as_str().as_bytes()).unwrap(),
                        reqwest::header::HeaderValue::from_bytes(v.as_bytes()).unwrap(),
                    )
                })
                .collect(),
        )
        .body(body_bytes.to_vec())
        .send()
        .await
        .map_err(|e| AppError::BadRequest(format!("peer unreachable: {e}")))?;

    let status =
        StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut builder = Response::builder().status(status);
    for (k, v) in resp.headers().iter() {
        let name_str = k.as_str().to_ascii_lowercase();
        if name_str == "transfer-encoding"
            || name_str == "content-length"
            || name_str == "content-encoding"
            || name_str == "connection"
        {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(k.as_str().as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("peer body: {e}")))?;
    builder
        .body(Body::from(bytes))
        .map_err(|e| AppError::Internal(anyhow::anyhow!("response build: {e}")))
}

fn should_forward_header(name: &HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    !matches!(
        n.as_str(),
        "host" | "cookie" | "authorization" | "content-length" | "connection" | "x-pier-peer-token"
    )
}

/// Background task: probe every registered peer on a 30s timer.
/// Agents have their own push-based heartbeat (`POST /api/v1/servers/heartbeat`),
/// so we only poll peers here.
pub fn spawn_heartbeat_task(state: SharedState) {
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            let peer_ids: Vec<String> = {
                let Ok(db) = state.db.lock() else {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                };
                db.prepare("SELECT id FROM servers WHERE kind = 'peer'")
                    .and_then(|mut stmt| {
                        stmt.query_map([], |row| row.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()
                    })
                    .unwrap_or_default()
            };
            for id in peer_ids {
                if let Err(e) = probe_peer(&state, &id).await {
                    tracing::debug!("Peer {id} probe failed: {e}");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

fn get_server_kind(state: &SharedState, id: &str) -> AppResult<String> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.query_row("SELECT kind FROM servers WHERE id = ?1", [id], |row| {
        row.get::<_, String>(0)
    })
    .map_err(|_| AppError::NotFound(format!("Server {id} not found")))
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
///
/// Identifies the server in two orderings, in this order:
///   1. sha256(body.agent_token) matches `agent_token_hash` — the new path,
///      used by every agent post-handshake.
///   2. body.agent_token matches plaintext `agent_token` — legacy fallback for
///      rows created before migration 40. On a successful legacy match we
///      lazily backfill `agent_token_hash` so the next heartbeat takes the
///      fast path and the plaintext column can eventually be retired.
pub async fn heartbeat(
    State(state): State<SharedState>,
    Json(body): Json<HeartbeatRequest>,
) -> AppResult<impl IntoResponse> {
    let token_hash = server_token::hash(&body.agent_token);

    // Resolve to a server row up front so we know which id to update and
    // whether we need to backfill the hash.
    let resolved: Option<(String, String, String, bool)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        // Try hash first.
        let by_hash = db
            .query_row(
                "SELECT id, name, status FROM servers WHERE agent_token_hash = ?1",
                [&token_hash],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .ok()
            .map(|(id, name, status)| (id, name, status, false));
        by_hash.or_else(|| {
            // Legacy fallback: rows from pre-migration-40 still carry the
            // plaintext token verbatim. The 4th tuple element flags this
            // path so we can backfill below.
            db.query_row(
                "SELECT id, name, status FROM servers
                 WHERE agent_token = ?1 AND agent_token <> '' AND agent_token_hash IS NULL",
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
            .map(|(id, name, status)| (id, name, status, true))
        })
    };

    let Some((server_id, server_name, prev_status, legacy)) = resolved else {
        return Err(AppError::Unauthorized);
    };

    let rows = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        if legacy {
            // Backfill the hash so future heartbeats take the fast path.
            // Plaintext column is left in place for one more release cycle
            // in case rollback is needed; a future migration nulls it.
            db.execute(
                "UPDATE servers
                 SET agent_token_hash = ?2,
                     status = 'online',
                     last_heartbeat = datetime('now'),
                     os_info = COALESCE(?3, os_info),
                     cpu_count = COALESCE(?4, cpu_count),
                     memory_total = COALESCE(?5, memory_total),
                     docker_version = COALESCE(?6, docker_version),
                     updated_at = datetime('now')
                 WHERE id = ?1",
                rusqlite::params![
                    server_id,
                    token_hash,
                    body.os_info,
                    body.cpu_count,
                    body.memory_total,
                    body.docker_version
                ],
            )?
        } else {
            db.execute(
                "UPDATE servers
                 SET status = 'online',
                     last_heartbeat = datetime('now'),
                     os_info = COALESCE(?2, os_info),
                     cpu_count = COALESCE(?3, cpu_count),
                     memory_total = COALESCE(?4, memory_total),
                     docker_version = COALESCE(?5, docker_version),
                     updated_at = datetime('now')
                 WHERE id = ?1",
                rusqlite::params![
                    server_id,
                    body.os_info,
                    body.cpu_count,
                    body.memory_total,
                    body.docker_version
                ],
            )?
        }
    };

    if rows == 0 {
        return Err(AppError::Unauthorized);
    }

    // Reuse the existing offline→online detection by reshaping the prev row
    // to the tuple the rest of this function expects.
    let prev = Some((server_id.clone(), server_name.clone(), prev_status));

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
            "SELECT id, name, kind, host, port, url, status, last_heartbeat, os_info,
                cpu_count, memory_total, docker_version, remote_version, last_error,
                is_local, created_at, country, city, country_code,
                agent_token_prefix, bootstrap_expires_at,
                CASE WHEN bootstrap_token_hash IS NULL THEN 0 ELSE 1 END AS bootstrap_pending
         FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "kind": row.get::<_, String>(2)?,
                    "host": row.get::<_, String>(3)?,
                    "port": row.get::<_, i64>(4)?,
                    "url": row.get::<_, Option<String>>(5)?,
                    // agent_token plaintext is no longer returned — only the
                    // 16-char fingerprint, so the operator can recognise the
                    // active credential without us shipping the secret over
                    // the wire on every page load.
                    "status": row.get::<_, String>(6)?,
                    "last_heartbeat": row.get::<_, Option<String>>(7)?,
                    "os_info": row.get::<_, Option<String>>(8)?,
                    "cpu_count": row.get::<_, Option<i64>>(9)?,
                    "memory_total": row.get::<_, Option<i64>>(10)?,
                    "docker_version": row.get::<_, Option<String>>(11)?,
                    "remote_version": row.get::<_, Option<String>>(12)?,
                    "last_error": row.get::<_, Option<String>>(13)?,
                    "is_local": row.get::<_, bool>(14)?,
                    "created_at": row.get::<_, String>(15)?,
                    "country": row.get::<_, Option<String>>(16)?,
                    "city": row.get::<_, Option<String>>(17)?,
                    "country_code": row.get::<_, Option<String>>(18)?,
                    "agent_token_prefix": row.get::<_, Option<String>>(19)?,
                    "bootstrap_expires_at": row.get::<_, Option<i64>>(20)?,
                    "bootstrap_pending": row.get::<_, i64>(21)? == 1,
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
    let (host, port, agent_token, is_local, _) = get_server_info(&state, &id)?;

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
    let (host, port, agent_token, is_local, _) = get_server_info(&state, &id)?;

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
    let (host, port, agent_token, is_local, _) = get_server_info(&state, &id)?;

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

/// GET /api/v1/servers/install-script — generate agent install script.
///
/// Embeds pier-core's self-signed cert PEM inline as a trust anchor for
/// `curl --cacert`, so the freshly installed agent can call back over HTTPS
/// without `-k` (insecure). If TLS is disabled on pier-core (env override),
/// falls back to plain `http://` with a warning comment in the script.
pub async fn install_script(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    // Bootstrap token is one-shot and short-lived (TTL 1h); the script
    // exchanges it for a long-term agent token via /handshake before
    // writing the systemd unit. `id` identifies the server row so the
    // handshake hits the right endpoint.
    let bootstrap_token = params.get("token").cloned().unwrap_or_default();
    let server_id = params.get("id").cloned().unwrap_or_default();
    if bootstrap_token.is_empty() {
        return Err(AppError::BadRequest("token parameter required".into()));
    }
    if server_id.is_empty() {
        return Err(AppError::BadRequest("id parameter required".into()));
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

    // Read pier-core's own cert so the agent can pin it via `curl --cacert`.
    // If the file is missing (e.g. TLS disabled at the env level), we degrade
    // gracefully to plain HTTP and the script prints a warning.
    let tls_enabled = state.config.tls_mode != crate::config::TlsMode::Off;
    let cert_pem = if tls_enabled {
        std::fs::read_to_string(state.config.tls_cert_dir.join("cert.pem")).ok()
    } else {
        None
    };

    let (scheme, cacert_block, cacert_arg) = match &cert_pem {
        Some(pem) => {
            // Heredoc body with the PEM. `'PIER_PEM'` (quoted) prevents shell
            // expansion of any unlikely `$`/backtick in the cert. Cert ends in
            // a trailing newline so the heredoc closes cleanly.
            let pem = pem.trim_end();
            let block = format!(
                "PIER_CACERT=$(mktemp)\n\
                 cat > \"$PIER_CACERT\" <<'PIER_PEM'\n\
                 {pem}\n\
                 PIER_PEM\n\
                 trap 'rm -f \"$PIER_CACERT\"' EXIT",
            );
            ("https", block, "--cacert \"$PIER_CACERT\"")
        }
        None => (
            "http",
            "# WARNING: pier-core TLS is disabled — calls are plaintext.".to_string(),
            "",
        ),
    };

    let script = format!(
        r#"#!/bin/bash
set -euo pipefail

# Pier Agent Installer
# Auto-generated by Pier
#
# This script pins pier-core's self-signed TLS certificate via `curl --cacert`.
# If pier-core regenerates its cert (e.g. operator deleted data/tls/cert.pem),
# re-download this installer from the UI and re-run on the agent host.

PIER_CORE_URL="{scheme}://{server_ip}:{pier_port}"
SERVER_ID="{server_id}"
BOOTSTRAP_TOKEN="{bootstrap_token}"
AGENT_PORT=3001

{cacert_block}

echo "=== Pier Agent Installer ==="
echo "Core server: $PIER_CORE_URL"
echo "Server id:   $SERVER_ID"

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
curl -fsSL {cacert_arg} "$PIER_CORE_URL/api/v1/health" >/dev/null 2>&1 || echo "Warning: Cannot reach Pier core"

# Try to download from GitHub release (uses public CA chain, no pinning needed)
DOWNLOAD_URL="https://github.com/joveptesg/Pier/releases/download/latest/pier-agent-linux-amd64"
curl -fsSL -o /opt/pier/bin/pier-agent "$DOWNLOAD_URL" || {{
    echo "Error: Could not download pier-agent"
    echo "Please build from source: cargo build --release -p pier-agent"
    exit 1
}}
chmod +x /opt/pier/bin/pier-agent

# 4. Handshake — spend the one-shot bootstrap for a long-term agent token.
#    The plaintext returned here is the only place the long-term token ever
#    exists outside the systemd Environment= file we're about to write.
echo "Performing handshake with Pier core..."
OS_INFO="$(uname -srm)"
DOCKER_VERSION="$(docker --version 2>/dev/null | cut -d' ' -f3 | tr -d ',' || echo unknown)"
HANDSHAKE_BODY=$(printf '{{"bootstrap_token":"%s","os_info":"%s","docker_version":"%s"}}' \
    "$BOOTSTRAP_TOKEN" "$OS_INFO" "$DOCKER_VERSION")

HANDSHAKE_RESPONSE=$(curl -fsSL {cacert_arg} \
    -X POST "$PIER_CORE_URL/api/v1/servers/$SERVER_ID/handshake" \
    -H "Content-Type: application/json" \
    -d "$HANDSHAKE_BODY" 2>&1) || {{
    echo "Error: handshake failed."
    echo "  Response: $HANDSHAKE_RESPONSE"
    echo "  The bootstrap token may have expired or already been used."
    echo "  Recreate the server in Pier UI to issue a fresh bootstrap, then re-run this script."
    exit 1
}}

# Extract agent_token without depending on jq — the field is the single
# string after `"agent_token":"…"` and contains only [A-Za-z0-9_].
AGENT_TOKEN=$(printf '%s' "$HANDSHAKE_RESPONSE" \
    | grep -oE '"agent_token":"[^"]+"' \
    | head -n 1 \
    | cut -d'"' -f4)

if [ -z "$AGENT_TOKEN" ]; then
    echo "Error: handshake response did not contain agent_token."
    echo "  Response: $HANDSHAKE_RESPONSE"
    exit 1
fi
echo "Handshake OK (agent token issued)."

# 5. Create systemd service with the long-term token. The bootstrap is now
#    invalid on core — even if this file leaks it, replaying the install
#    script will fail at handshake.
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
chmod 600 /etc/systemd/system/pier-agent.service

# 6. Start agent
systemctl daemon-reload
systemctl enable --now pier-agent

echo ""
echo "=== Pier Agent installed ==="
echo "Agent port: $AGENT_PORT"
echo "Status: systemctl status pier-agent"
echo "Logs:   journalctl -u pier-agent -f"

# 7. Confirm liveness — first heartbeat. /handshake already marked us
#    online, this is a sanity check that the running agent can reach core
#    with the new long-term token.
sleep 2
curl -fsS {cacert_arg} -X POST "$PIER_CORE_URL/api/v1/servers/heartbeat" \
    -H "Content-Type: application/json" \
    -d "$(printf '{{"agent_token":"%s","os_info":"%s","docker_version":"%s"}}' \
            "$AGENT_TOKEN" "$OS_INFO" "$DOCKER_VERSION")" \
    || echo "Warning: first heartbeat failed; agent will retry."

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
/// Returns (host, port, agent_token, is_local, kind). For peer kind, `host`/`port`
/// are empty/0 and callers should route through the proxy handler instead.
fn get_server_info(
    state: &SharedState,
    id: &str,
) -> Result<(String, i64, String, bool, String), AppError> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.query_row(
        "SELECT host, port, agent_token, is_local, kind FROM servers WHERE id = ?1",
        [id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, bool>(3)?,
                row.get::<_, String>(4)?,
            ))
        },
    )
    .map_err(|_| AppError::NotFound(format!("Server {id} not found")))
}
