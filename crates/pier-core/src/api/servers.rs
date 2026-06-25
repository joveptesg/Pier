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
    /// Re-affirm the agent's TLS leaf fingerprint (lowercase hex). Lets core
    /// re-pin after a cert regeneration without a fresh bootstrap; NULL keeps
    /// the existing pin.
    pub agent_tls_fingerprint: Option<String>,
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
                country, city, country_code,
                CASE WHEN federation_token IS NULL OR federation_token = ''
                     THEN 0 ELSE 1 END AS federation_paired
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
                "federation_paired": row.get::<_, i64>(19)? == 1,
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
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.name_required",
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();

    match body.kind.as_str() {
        KIND_AGENT => {
            // Strip [...] brackets so IPv6 literals enter the DB in
            // the bare form. URL/endpoint formatters add brackets back
            // at use time — see `crate::network::address`.
            let host = body
                .host
                .as_deref()
                .map(crate::network::address::normalize_host)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::BadRequest(crate::i18n::te("errors.servers.host_required_for_agent"))
                })?;
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
                .ok_or_else(|| {
                    AppError::BadRequest(crate::i18n::te("errors.servers.url_required_for_peer"))
                })?
                .to_string();
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(AppError::BadRequest(crate::i18n::te(
                    "errors.servers.url_must_be_http",
                )));
            }
            let token = body
                .api_token
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::BadRequest(crate::i18n::te(
                        "errors.servers.api_token_required_for_peer",
                    ))
                })?
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
        other => Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.servers.unknown_kind_expected",
            &[("kind", other)],
        ))),
    }
}

/// POST /api/v1/servers/{id}/rotate
///
/// Mints a fresh `pier_srv_…` long-term token, asks the agent to swap
/// it in via `/api/v1/agent/auth/rotate` (authenticated with the OLD
/// token), and persists the new hash+plaintext on core once the agent
/// acks 200. The agent then exits and systemd respawns it with the new
/// `PIER_AGENT_TOKEN` in its environment, so the OLD token is dead the
/// next time anyone tries to use it.
///
/// Why we keep the plaintext column even after hashing landed in
/// migration 40: core needs to authenticate OUTBOUND to the agent
/// using a Bearer that the agent itself compares with `==`. The hash
/// alone can't reproduce the plaintext, so we trade a slightly weaker
/// at-rest posture for a much simpler outbound auth story. A future
/// migration moves outbound auth to a per-agent signing key derived
/// from a master secret in `data_dir`, at which point the plaintext
/// column can finally be nulled out.
///
/// Mesh-routed: once configure_mesh has flipped this peer to `active`,
/// `get_server_info` returns the mesh IP so the POST below goes over
/// WireGuard. Pre-mesh rotations work the same way over the public
/// IP — the endpoint is identical.
pub async fn rotate_token(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let outcome = rotate_token_internal(&state, &id).await?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "token_rotated_at": outcome.rotated_at,
        "agent_token_prefix": outcome.prefix,
    })))
}

/// Result of a successful rotation. Returned to both the HTTP handler
/// (wrapped in JSON) and to the scheduled rotator (for logging).
#[derive(Debug, Clone)]
pub struct RotationOutcome {
    pub rotated_at: i64,
    pub prefix: String,
}

/// Core of [`rotate_token`], usable from contexts without axum
/// extractors (e.g. the periodic rotation scheduler in
/// [`auth::rotation`]). All preconditions and side effects are
/// identical to the handler: agent-kind only, non-local, agent must
/// be reachable, DB write happens only after the agent ack.
pub async fn rotate_token_internal(state: &SharedState, id: &str) -> AppResult<RotationOutcome> {
    // Resolve the current server (host honors mesh-IP preference set in
    // 0.3e). Only agent-kind makes sense here — peers carry their own
    // user-issued grant token that this endpoint doesn't own.
    let AgentConn {
        host,
        port,
        token: current_token,
        is_local,
        kind,
        tls_fingerprint,
    } = get_server_info(state, id)?;
    if kind != KIND_AGENT {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.rotate_agent_only",
        )));
    }
    if is_local {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.local_no_token_to_rotate",
        )));
    }
    if current_token.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.no_active_token",
        )));
    }

    // Mint the new token. Hash now so we never write the plaintext
    // anywhere except the agent's env file (and `servers.agent_token`,
    // for outbound auth — same trade-off as above).
    let next = server_token::generate_agent();
    let next_hash = server_token::hash(&next.plaintext);

    // Push to the agent BEFORE we update the DB. If the agent never
    // sees the new token (network blip, agent down, helper rejected
    // the file write), we don't want core thinking it succeeded.
    let url = format!(
        "https://{}/api/v1/agent/auth/rotate",
        crate::network::address::authority(&host, port)
    );
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        std::time::Duration::from_secs(10),
    )
    .map_err(AppError::Internal)?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {current_token}"))
        .json(&serde_json::json!({"new_token": next.plaintext}))
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.agent_unreachable_at",
                &[("url", &url), ("error", &e.to_string())],
            ))
        })?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.servers.agent_refused_rotation",
            &[("status", &status.to_string()), ("body", &body)],
        )));
    }

    // Persist the new token. We bump token_version so the UI can show
    // monotonic rotation history and so two concurrent rotation clicks
    // would race in a detectable way (the second one would observe a
    // version it didn't expect).
    let now = chrono::Utc::now().timestamp();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE servers
             SET agent_token = ?1,
                 agent_token_hash = ?2,
                 agent_token_prefix = ?3,
                 token_rotated_at = ?4,
                 token_version = token_version + 1,
                 updated_at = datetime('now')
             WHERE id = ?5",
            rusqlite::params![next.plaintext, next_hash, next.prefix, now, id],
        )?;
    }

    Ok(RotationOutcome {
        rotated_at: now,
        prefix: next.prefix,
    })
}

#[derive(Deserialize)]
pub struct HandshakeRequest {
    /// `pier_boot_…` plaintext from the install command.
    pub bootstrap_token: String,
    /// Best-effort host facts so the operator sees a populated server card
    /// without waiting for the first heartbeat round-trip.
    pub os_info: Option<String>,
    pub docker_version: Option<String>,
    /// SHA-256 (lowercase hex) of the agent's TLS leaf cert. The installer
    /// computes this from the cert it pre-generated; core pins it so every
    /// subsequent core→agent call validates the agent's identity.
    pub agent_tls_fingerprint: Option<String>,
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
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.bootstrap_token_required",
        )));
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
                 agent_tls_fingerprint = COALESCE(?8, agent_tls_fingerprint),
                 token_rotated_at = ?9,
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
                body.agent_tls_fingerprint,
                now,
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
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.server_not_found_or_local",
        )));
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
            let AgentConn {
                host,
                port,
                token: agent_token,
                tls_fingerprint,
                ..
            } = get_server_info(&state, &id)?;
            let url = format!(
                "https://{}/health",
                crate::network::address::authority(&host, port)
            );
            let client = crate::network::agent_client::build_agent_client(
                tls_fingerprint.as_deref(),
                std::time::Duration::from_secs(5),
            )
            .map_err(AppError::Internal)?;
            let resp = client
                .get(&url)
                .header("Authorization", format!("Bearer {agent_token}"))
                .send()
                .await
                .map_err(|e| {
                    AppError::BadRequest(crate::i18n::te_args(
                        "errors.servers.cannot_connect_agent_at",
                        &[("url", &url), ("error", &e.to_string())],
                    ))
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
                Err(AppError::BadRequest(crate::i18n::te_args(
                    "errors.servers.agent_responded_status",
                    &[("status", &resp.status().to_string())],
                )))
            }
        }
        KIND_LOCAL => Ok(Json(
            serde_json::json!({"ok": true, "kind": "local", "message": "Local core"}),
        )),
        other => Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.servers.unknown_kind",
            &[("kind", other)],
        ))),
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
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.servers.peer_not_found",
                &[("id", id)],
            ))
        })?
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
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.servers.peer_not_found",
                &[("id", &id)],
            ))
        })?
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
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.body_read_failed",
                &[("error", &e.to_string())],
            ))
        })?;

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
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.peer_unreachable",
                &[("error", &e.to_string())],
            ))
        })?;

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
    .map_err(|_| {
        AppError::NotFound(crate::i18n::te_args(
            "errors.servers.server_not_found",
            &[("id", id)],
        ))
    })
}

/// GET /api/v1/servers/{id}/metrics
pub async fn metrics(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (host, port, agent_token, tls_fingerprint) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host, port, agent_token, agent_tls_fingerprint FROM servers WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.servers.server_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let url = format!(
        "https://{}/metrics",
        crate::network::address::authority(&host, port)
    );
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        std::time::Duration::from_secs(5),
    )
    .map_err(AppError::Internal)?;

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.agent_unreachable",
                &[("error", &e.to_string())],
            ))
        })?;

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
                     agent_tls_fingerprint = COALESCE(?7, agent_tls_fingerprint),
                     updated_at = datetime('now')
                 WHERE id = ?1",
                rusqlite::params![
                    server_id,
                    token_hash,
                    body.os_info,
                    body.cpu_count,
                    body.memory_total,
                    body.docker_version,
                    body.agent_tls_fingerprint
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
                     agent_tls_fingerprint = COALESCE(?6, agent_tls_fingerprint),
                     updated_at = datetime('now')
                 WHERE id = ?1",
                rusqlite::params![
                    server_id,
                    body.os_info,
                    body.cpu_count,
                    body.memory_total,
                    body.docker_version,
                    body.agent_tls_fingerprint
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

#[derive(Deserialize)]
pub struct SetFederationTokenRequest {
    /// Plaintext token the operator copied from the peer's UI.
    /// Empty string clears the field (effectively un-pairs).
    pub token: String,
}

/// PUT /api/v1/servers/{id}/federation-token
///
/// Primary-side companion to migration 52. Stores the plaintext
/// federation token the operator copied from the peer's federation
/// settings page. Only meaningful for `kind='peer'` rows — the peer
/// is what mints federation tokens; an agent never does.
///
/// Validation is intentionally light: the actual proof that the token
/// works happens when the next federation write call returns 200.
/// Storing a garbage value here only breaks future writes, it doesn't
/// expose anything.
pub async fn set_federation_token(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<SetFederationTokenRequest>,
) -> AppResult<impl IntoResponse> {
    let token = body.token.trim().to_string();
    let store_value = if token.is_empty() { None } else { Some(token) };

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute(
        "UPDATE servers SET federation_token = ?1, updated_at = datetime('now') \
         WHERE id = ?2 AND kind = 'peer'",
        rusqlite::params![store_value, id],
    )?;
    if rows == 0 {
        return Err(AppError::NotFound(crate::i18n::te_args(
            "errors.servers.peer_not_found_federation",
            &[("id", &id)],
        )));
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "paired": store_value.is_some(),
    })))
}

/// PUT /api/v1/servers/{id}/name — rename server.
pub async fn rename(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<RenameServerRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.name_required",
        )));
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
        return Err(AppError::NotFound(crate::i18n::te_args(
            "errors.servers.server_not_found",
            &[("id", &id)],
        )));
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
                CASE WHEN bootstrap_token_hash IS NULL THEN 0 ELSE 1 END AS bootstrap_pending,
                token_rotated_at, token_version,
                CASE WHEN federation_token IS NULL OR federation_token = ''
                     THEN 0 ELSE 1 END AS federation_paired
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
                    "token_rotated_at": row.get::<_, Option<i64>>(22)?,
                    "token_version": row.get::<_, i64>(23)?,
                    "federation_paired": row.get::<_, i64>(24)? == 1,
                }))
            },
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.servers.server_not_found",
                &[("id", &id)],
            ))
        })?;

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
    let AgentConn {
        host,
        port,
        token: agent_token,
        is_local,
        tls_fingerprint,
        ..
    } = get_server_info(&state, &id)?;

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
    let url = format!(
        "https://{}/api/v1/agent/status",
        crate::network::address::authority(&host, port)
    );
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        std::time::Duration::from_secs(10),
    )
    .map_err(AppError::Internal)?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.agent_unreachable",
                &[("error", &e.to_string())],
            ))
        })?;
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
    let AgentConn {
        host,
        port,
        token: agent_token,
        is_local,
        tls_fingerprint,
        ..
    } = get_server_info(&state, &id)?;

    if is_local {
        // Deploy locally
        let stack_name = body["stack_name"].as_str().ok_or_else(|| {
            AppError::BadRequest(crate::i18n::te("errors.servers.stack_name_required"))
        })?;
        let compose_yaml = body["compose_yaml"].as_str().ok_or_else(|| {
            AppError::BadRequest(crate::i18n::te("errors.servers.compose_yaml_required"))
        })?;
        let output =
            crate::docker::compose::deploy_stack(stack_name, compose_yaml, &state.config, None)
                .await
                .map_err(AppError::Internal)?;
        return Ok(Json(serde_json::json!({"ok": true, "output": output})));
    }

    // Proxy to remote agent
    let url = format!(
        "https://{}/api/v1/agent/deploy",
        crate::network::address::authority(&host, port)
    );
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        std::time::Duration::from_secs(120),
    )
    .map_err(AppError::Internal)?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.agent_unreachable",
                &[("error", &e.to_string())],
            ))
        })?;
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
    let AgentConn {
        host,
        port,
        token: agent_token,
        is_local,
        tls_fingerprint,
        ..
    } = get_server_info(&state, &id)?;

    if is_local {
        let stack_name = body["stack_name"].as_str().ok_or_else(|| {
            AppError::BadRequest(crate::i18n::te("errors.servers.stack_name_required"))
        })?;
        let output = crate::docker::compose::down_stack(stack_name, &state.config)
            .await
            .map_err(AppError::Internal)?;
        return Ok(Json(serde_json::json!({"ok": true, "output": output})));
    }

    let url = format!(
        "https://{}/api/v1/agent/stop",
        crate::network::address::authority(&host, port)
    );
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        std::time::Duration::from_secs(30),
    )
    .map_err(AppError::Internal)?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.servers.agent_unreachable",
                &[("error", &e.to_string())],
            ))
        })?;
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
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.token_param_required",
        )));
    }
    if server_id.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.servers.id_param_required",
        )));
    }

    // Get Pier server's public IP and port. Prefers IPv4; falls back
    // to IPv6 when v4 isn't known so a dual-stack core with a v4-only
    // entry still gets a sensible install command. Brackets are
    // applied by `network::address::authority` below.
    let server_ip = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let v4: Option<String> = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'server.public_ipv4'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .or_else(|| {
                db.query_row(
                    "SELECT value FROM settings WHERE key = 'server.public_ip'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            });
        v4.or_else(|| {
            db.query_row(
                "SELECT value FROM settings WHERE key = 'server.public_ipv6'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
        })
        .unwrap_or_else(|| "YOUR_PIER_SERVER_IP".to_string())
    };
    let pier_port = state.config.port;
    let server_authority = crate::network::address::authority(&server_ip, pier_port.into());

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

PIER_CORE_URL="{scheme}://{server_authority}"
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

# 3b. Drop pier-net-helper (dormant). The helper is the privileged hook
#     that pier-agent uses later to bring up a WireGuard mesh from the UI
#     ("Enable Mesh" wizard). It does NOTHING on its own — no `wg`, no
#     `apt install wireguard`, no `wg-quick up` — until pier-core sends
#     an explicit op over /run/pier/net.sock. Failing to install it here
#     is non-fatal: the agent works without mesh; the operator can run
#     /install-helper.sh later to retrofit.
echo "Installing pier-net-helper (dormant)..."
# WireGuard tools must be present on the host — the sandboxed helper
# (ProtectSystem=strict) cannot apt-get them itself; it only verifies them.
DEBIAN_FRONTEND=noninteractive apt-get install -y wireguard wireguard-tools >/dev/null 2>&1 \
    || echo "Warning: apt install wireguard failed; mesh will be unavailable until installed."
HELPER_URL="https://github.com/joveptesg/Pier/releases/download/latest/pier-net-helper-linux-amd64"
if curl -fsSL -o /usr/local/bin/pier-net-helper "$HELPER_URL"; then
    chmod 0755 /usr/local/bin/pier-net-helper
    cat > /etc/systemd/system/pier-net-helper.service <<HELPER_UNIT
[Unit]
Description=Pier Network Helper (privileged WireGuard mesh operations)
Documentation=https://github.com/joveptesg/Pier
After=network-pre.target
Before=pier-agent.service

[Service]
Type=simple
ExecStart=/usr/local/bin/pier-net-helper
Restart=on-failure
RestartSec=2
User=root
Group=root
RuntimeDirectory=pier
RuntimeDirectoryMode=0750
ProtectSystem=strict
ReadWritePaths=-/etc/wireguard /run/pier
ProtectHome=true
PrivateTmp=true
NoNewPrivileges=true
ProtectKernelLogs=true
ProtectKernelTunables=true
ProtectControlGroups=true
RestrictNamespaces=true
LockPersonality=true
MemoryDenyWriteExecute=true
SystemCallArchitectures=native
AmbientCapabilities=CAP_NET_ADMIN CAP_SYS_MODULE
CapabilityBoundingSet=CAP_NET_ADMIN CAP_SYS_MODULE

[Install]
WantedBy=multi-user.target
HELPER_UNIT
    chmod 644 /etc/systemd/system/pier-net-helper.service
    # The helper writes wg0.conf + wg0.privkey under /etc/wireguard. systemd's
    # ReadWritePaths can only make that path writable if it EXISTS when the unit
    # starts, so create it now (the helper's own sandbox can't mkdir in /etc).
    mkdir -p /etc/wireguard && chmod 700 /etc/wireguard
    systemctl daemon-reload
    systemctl enable --now pier-net-helper.service || \
        echo "Warning: pier-net-helper.service failed to start; mesh features will be unavailable."
else
    echo "Warning: pier-net-helper binary unavailable; mesh features will be unavailable until you re-run with a working release."
fi

# 3c. Generate the agent's own TLS certificate. The core→agent channel is
#     HTTPS; core pins this cert's SHA-256 leaf fingerprint (no PKI chain or
#     hostname validation — agents are reached by raw IP / mesh IP), so a
#     minimal self-signed cert suffices. We compute the fingerprint here and
#     hand it to core in the handshake so it can pin it from the first call.
echo "Generating agent TLS certificate..."
AGENT_TLS_DIR=/etc/pier-agent/tls
mkdir -p "$AGENT_TLS_DIR"
if [ ! -s "$AGENT_TLS_DIR/cert.pem" ] || [ ! -s "$AGENT_TLS_DIR/key.pem" ]; then
    if ! openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$AGENT_TLS_DIR/key.pem" -out "$AGENT_TLS_DIR/cert.pem" \
        -days 3650 -subj "/CN=pier-agent" >/dev/null 2>&1; then
        echo "Error: openssl failed to generate the agent TLS cert."
        exit 1
    fi
    chmod 600 "$AGENT_TLS_DIR/key.pem"
fi
AGENT_FP=$(openssl x509 -in "$AGENT_TLS_DIR/cert.pem" -outform DER 2>/dev/null | sha256sum | cut -d' ' -f1)
if [ -z "$AGENT_FP" ]; then
    echo "Error: could not compute the agent TLS fingerprint (is openssl installed?)."
    exit 1
fi
echo "Agent TLS fingerprint: $AGENT_FP"

# 4. Handshake — spend the one-shot bootstrap for a long-term agent token.
#    The plaintext returned here is the only place the long-term token ever
#    exists outside the systemd Environment= file we're about to write.
echo "Performing handshake with Pier core..."
OS_INFO="$(uname -srm)"
DOCKER_VERSION="$(docker --version 2>/dev/null | cut -d' ' -f3 | tr -d ',' || echo unknown)"
HANDSHAKE_BODY=$(printf '{{"bootstrap_token":"%s","os_info":"%s","docker_version":"%s","agent_tls_fingerprint":"%s"}}' \
    "$BOOTSTRAP_TOKEN" "$OS_INFO" "$DOCKER_VERSION" "$AGENT_FP")

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

# 5. Write the long-term token to a private env file the agent can
#    rewrite later during rotation, then create a systemd unit that
#    pulls it via EnvironmentFile=. This indirection matters: if we
#    embedded the token directly with Environment="PIER_AGENT_TOKEN=…",
#    rotation would have to rebuild the unit file (and `systemctl
#    daemon-reload`), which is heavier and easier to break than
#    overwriting a single line in a file the agent owns.
mkdir -p /etc/pier-agent
cat > /etc/pier-agent/auth.env <<ENV
PIER_AGENT_TOKEN=$AGENT_TOKEN
ENV
chmod 600 /etc/pier-agent/auth.env

cat > /etc/systemd/system/pier-agent.service <<UNIT
[Unit]
Description=Pier Agent
After=network.target docker.service
Requires=docker.service

[Service]
Type=simple
EnvironmentFile=/etc/pier-agent/auth.env
Environment="PIER_AGENT_PORT=$AGENT_PORT"
Environment="PIER_AGENT_DATA_DIR=/var/lib/pier-agent"
Environment="PIER_AGENT_TLS_DIR=$AGENT_TLS_DIR"
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

# 6b. Firewall — allow SSH, the agent API port, and WireGuard, then enable ufw.
#     SSH is detected + allowed FIRST so this can't lock you out. Opt out with
#     PIER_SKIP_FIREWALL=1 in the environment.
if [ "${{PIER_SKIP_FIREWALL:-0}}" != "1" ] && command -v ufw >/dev/null 2>&1; then
    SSH_PORT=$(sshd -T 2>/dev/null | awk '/^port /{{print $2; exit}}')
    SSH_PORT=${{SSH_PORT:-22}}
    ufw allow "$SSH_PORT"/tcp >/dev/null 2>&1 || true
    ufw allow 22/tcp >/dev/null 2>&1 || true
    ufw allow "$AGENT_PORT"/tcp >/dev/null 2>&1 || true
    ufw allow 51820/udp >/dev/null 2>&1 || true
    ufw --force enable >/dev/null 2>&1 \
        && echo "Firewall enabled (ssh:$SSH_PORT, agent:$AGENT_PORT, mesh:51820/udp)" \
        || echo "Warning: ufw enable failed; review the host firewall manually."
fi

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
    -d "$(printf '{{"agent_token":"%s","os_info":"%s","docker_version":"%s","agent_tls_fingerprint":"%s"}}' \
            "$AGENT_TOKEN" "$OS_INFO" "$DOCKER_VERSION" "$AGENT_FP")" \
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

/// Resolved connection info for a server, used by every core→agent call.
///
/// `host`/`port` honor the mesh-IP preference (once a peer's WireGuard tunnel
/// is `active`, outbound traffic flips onto the private IP). `tls_fingerprint`
/// is the agent's pinned leaf-cert SHA-256 (lowercase hex), `None` until the
/// handshake has delivered it — callers pass it to
/// [`crate::network::agent_client::build_agent_client`]. For `kind = "peer"`,
/// `host`/`port` are empty/0 and callers route through the proxy handler.
pub(crate) struct AgentConn {
    pub host: String,
    pub port: i64,
    pub token: String,
    pub is_local: bool,
    pub kind: String,
    pub tls_fingerprint: Option<String>,
}

/// Helper: extract server connection info. See [`AgentConn`].
pub(crate) fn get_server_info(state: &SharedState, id: &str) -> Result<AgentConn, AppError> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Prefer the mesh IP when this server's wireguard_peers row is
    // `active` and the global mesh is enabled. This is what flips
    // outbound core→agent traffic onto the encrypted tunnel once
    // `configure_mesh` finishes: the public IP stops being used for
    // any agent ops, even though core continues to listen on it for
    // incoming heartbeats during the transition (see migration plan
    // 0.3f for the firewall close).
    //
    // Fallback to the public host if mesh isn't fully up yet so a
    // mid-rollout configure_mesh still works — the dispatcher uses
    // this same function to reach the very peer it's bringing online.
    let row = db
        .query_row(
            "SELECT s.host, s.port, s.agent_token, s.is_local, s.kind,
                    wp.assigned_ip, wp.status, wc.enabled, s.agent_tls_fingerprint
             FROM servers s
             LEFT JOIN wireguard_peers wp ON wp.server_id = s.id
             LEFT JOIN wireguard_config wc ON wc.id = 1
             WHERE s.id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.servers.server_not_found",
                &[("id", id)],
            ))
        })?;

    let (
        mut host,
        port,
        token,
        is_local,
        kind,
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
    Ok(AgentConn {
        host,
        port,
        token,
        is_local,
        kind,
        tls_fingerprint,
    })
}
