//! Core↔Core federation endpoints (Mode 2 of the multi-server plan).
//!
//! Two sides:
//! * `peers` — outgoing: remote pier-core instances this node can control.
//! * `peer_grants` — incoming: tokens that authorize remote pier-core instances
//!   to control this node (checked by the auth middleware).
//!
//! Both sides live on every pier-core (symmetric). No dedicated agent binary.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::{AuthUser, PEER_TOKEN_HEADER};
use crate::catalog;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

// ── Outgoing: peer_cores (this instance controls remote pier-cores) ────────

#[derive(Deserialize)]
pub struct CreatePeerRequest {
    pub name: String,
    pub url: String,
    pub api_token: String,
}

/// GET /api/v1/peers — list registered remote cores.
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, url, status, last_heartbeat, remote_version, last_error, created_at
         FROM peer_cores ORDER BY created_at ASC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "url": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "last_heartbeat": row.get::<_, Option<String>>(4)?,
                "remote_version": row.get::<_, Option<String>>(5)?,
                "last_error": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, String>(7)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/peers — register a remote core we intend to control.
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreatePeerRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    let url = body.url.trim().trim_end_matches('/').to_string();
    let token = body.api_token.trim().to_string();
    if name.is_empty() || url.is_empty() || token.is_empty() {
        return Err(AppError::BadRequest(
            "name, url, api_token are required".into(),
        ));
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err(AppError::BadRequest(
            "url must start with http:// or https://".into(),
        ));
    }

    let id = uuid::Uuid::new_v4().to_string();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO peer_cores (id, name, url, api_token) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, name, url, token],
        )?;
    }

    // Probe synchronously so the first status line in the UI is meaningful.
    let _ = probe_peer(&state, &id).await;

    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// GET /api/v1/peers/{id} — detail.
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let item = db
        .query_row(
            "SELECT id, name, url, status, last_heartbeat, remote_version, last_error, created_at
             FROM peer_cores WHERE id = ?1",
            [&id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "url": row.get::<_, String>(2)?,
                    "status": row.get::<_, String>(3)?,
                    "last_heartbeat": row.get::<_, Option<String>>(4)?,
                    "remote_version": row.get::<_, Option<String>>(5)?,
                    "last_error": row.get::<_, Option<String>>(6)?,
                    "created_at": row.get::<_, String>(7)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Peer {id} not found")))?;
    Ok(Json(item))
}

/// DELETE /api/v1/peers/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM peer_cores WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Peer {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct RenamePeerRequest {
    pub name: String,
}

/// PUT /api/v1/peers/{id}/name
pub async fn rename(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RenamePeerRequest>,
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
        "UPDATE peer_cores SET name = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![name, id],
    )?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Peer {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true, "name": name})))
}

/// POST /api/v1/peers/{id}/test — synchronous connectivity probe.
pub async fn test(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    match probe_peer(&state, &id).await {
        Ok(info) => Ok(Json(serde_json::json!({"ok": true, "peer": info}))),
        Err(e) => Err(e),
    }
}

/// Fetch `/api/v1/peers/probe` on the remote core, update local status.
pub(crate) async fn probe_peer(state: &SharedState, id: &str) -> AppResult<serde_json::Value> {
    let (url, token) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT url, api_token FROM peer_cores WHERE id = ?1",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Peer {id} not found")))?
    };

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("HTTP client: {e}")))?;

    let probe_url = format!("{url}/api/v1/peers/probe");
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
                "UPDATE peer_cores SET status = 'online', last_heartbeat = datetime('now'),
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

/// Background task: probe every registered peer core on a timer.
/// Spawned from main.rs at startup; runs for the lifetime of the process.
pub fn spawn_heartbeat_task(state: SharedState) {
    tokio::spawn(async move {
        // Small initial delay so startup logs don't race with DB migrations.
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        loop {
            let peer_ids: Vec<String> = {
                let Ok(db) = state.db.lock() else {
                    tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                    continue;
                };
                let rows = db
                    .prepare("SELECT id FROM peer_cores")
                    .and_then(|mut stmt| {
                        stmt.query_map([], |row| row.get::<_, String>(0))?
                            .collect::<Result<Vec<_>, _>>()
                    });
                rows.unwrap_or_default()
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

fn mark_peer_error(state: &SharedState, id: &str, msg: &str) -> AppResult<()> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "UPDATE peer_cores SET status = 'offline', last_error = ?2, updated_at = datetime('now')
         WHERE id = ?1",
        rusqlite::params![id, msg],
    )?;
    Ok(())
}

/// Proxy any request to the remote core's API.
/// Route: `/api/v1/peers/{id}/proxy/{*path}` — captures remaining path after `/proxy/`.
/// Prepends `/api/v1/` on the remote side, injects the peer token, passes method + body + query.
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
            "SELECT url, api_token FROM peer_cores WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| AppError::NotFound(format!("Peer {id} not found")))?
    };

    let method = req.method().clone();
    let query = req.uri().query().map(|s| s.to_string());
    let mut target = format!("{url}/api/v1/{rest}");
    if let Some(q) = query {
        target.push('?');
        target.push_str(&q);
    }

    // Preserve a safe subset of incoming headers.
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
        // Hop-by-hop + content-encoding headers are unsafe to forward verbatim.
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
    // Skip browser/session specific headers.
    !matches!(
        n.as_str(),
        "host" | "cookie" | "authorization" | "content-length" | "connection" | "x-pier-peer-token"
    )
}

// ── Incoming: peer_grants (external cores authorized to control this node) ──

/// GET /api/v1/peers/probe — called by a remote core to verify its grant.
/// Auth: X-Pier-Peer-Token header (validated by require_auth middleware).
pub async fn probe(axum::Extension(user): axum::Extension<AuthUser>) -> impl IntoResponse {
    Json(serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "role": user.role,
        "principal": user.username,
    }))
}

#[derive(Deserialize)]
pub struct CreateGrantRequest {
    pub name: String,
}

/// GET /api/v1/peer-grants — list grants this core exposes.
pub async fn grants_list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, is_active, last_used_at, last_used_ip, created_at
         FROM peer_grants ORDER BY created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "is_active": row.get::<_, bool>(2)?,
                "last_used_at": row.get::<_, Option<String>>(3)?,
                "last_used_ip": row.get::<_, Option<String>>(4)?,
                "created_at": row.get::<_, String>(5)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/peer-grants — create a new grant and reveal its token once.
pub async fn grants_create(
    State(state): State<SharedState>,
    Json(body): Json<CreateGrantRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest("Name is required".into()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let token = catalog::generate_password(48);

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO peer_grants (id, name, token) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, name, token],
    )?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "id": id,
        "name": name,
        "token": token,
    })))
}

/// DELETE /api/v1/peer-grants/{id}
pub async fn grants_revoke(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM peer_grants WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Grant {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}
