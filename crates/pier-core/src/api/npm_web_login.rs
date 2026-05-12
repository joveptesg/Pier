//! `npm login --auth-type=web` flow.
//!
//! Three endpoints make up the dance:
//!   1. CLI:   `POST /registry/npm/-/v1/login`           — opens a session.
//!   2. CLI:   `GET  /registry/npm/-/v1/done/{id}`       — polls for the token.
//!   3. Panel: `POST /api/v1/account/cli-login/{id}/authorize`
//!      — user approves the CLI.
//!
//! The panel handler is gated by the normal session-cookie auth (incl. 2FA);
//! the public ones aren't, because the CLI hasn't authenticated yet. Sessions
//! self-expire after 10 minutes (`expires_at`) and an authorised session is
//! cleared the moment the CLI consumes the plaintext token.
//!
//! Stored plaintext is encrypted via `crate::crypto` — the rest of `api_tokens`
//! only ever holds the sha256 hash, so the encrypted column closes the obvious
//! "DB dump leaks live tokens" gap.
//!
//! TTL housekeeping (`sweep_expired_sessions`) is best-effort; called from
//! `main` on startup and on a daily timer alongside the other retention jobs.

use std::time::Duration;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::{HOST, USER_AGENT};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use tokio::time::sleep;

use crate::auth::api_token;
use crate::auth::middleware::AuthUser;
use crate::crypto;
use crate::error::AppError;
use crate::state::SharedState;

const SESSION_TTL_SECONDS: i64 = 600;

/// Public CLI-facing endpoints. Mount under the registry router (`/registry/npm/...`)
/// so the URLs the CLI sees match the npm spec.
pub fn public_router() -> Router<SharedState> {
    Router::new()
        .route("/-/v1/login", post(begin_login))
        .route("/-/v1/done/{session_id}", get(poll_done))
}

/// Panel-facing endpoints. Mounted under `/api/v1/account/` so they pick up
/// the existing session-cookie auth + 2FA.
pub fn protected_router() -> Router<SharedState> {
    Router::new()
        .route("/account/cli-login/{session_id}", get(session_status))
        .route(
            "/account/cli-login/{session_id}/authorize",
            post(authorize_session),
        )
}

// ----- request/response types -------------------------------------------------

#[derive(Debug, Deserialize)]
struct BeginBody {
    hostname: Option<String>,
}

#[derive(Debug, Serialize)]
struct BeginResponse {
    #[serde(rename = "loginUrl")]
    login_url: String,
    #[serde(rename = "doneUrl")]
    done_url: String,
}

#[derive(Debug, Serialize)]
struct PollAuthorized {
    token: String,
}

#[derive(Debug, Serialize)]
struct SessionStatus {
    session_id: String,
    hostname: String,
    status: String,
    peer_ip: Option<String>,
    user_agent: Option<String>,
    expires_at: i64,
}

#[derive(Debug, Deserialize, Default)]
struct AuthorizeBody {
    /// Optional friendly name shown next to the token in the UI. Defaults to
    /// `npm web login (<hostname>)`.
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuthorizeResponse {
    ok: bool,
    /// Plaintext token. Returned here so the UI can display it as a fallback
    /// "copy this and put it in .npmrc" if the CLI poll has timed out.
    token: String,
}

// ----- public CLI endpoints ---------------------------------------------------

async fn begin_login(
    State(state): State<SharedState>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<BeginBody>,
) -> Result<Json<BeginResponse>, AppError> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let hostname = body.hostname.unwrap_or_default();
    let now = chrono::Utc::now().timestamp();
    let expires_at = now + SESSION_TTL_SECONDS;
    let peer_ip = addr.ip().to_string();
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let session_id_db = session_id.clone();
    let hostname_db = hostname.clone();
    let state_cl = state.clone();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.execute(
            "INSERT INTO npm_login_sessions
                (session_id, hostname, status, peer_ip, user_agent, created_at, expires_at)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6)",
            rusqlite::params![
                session_id_db,
                hostname_db,
                peer_ip,
                user_agent,
                now,
                expires_at
            ],
        )
        .map_err(|e| AppError::Internal(anyhow::anyhow!("insert session: {e}")))?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))??;

    let base = public_base_url(&headers);
    tracing::info!("npm web-login: session {session_id} opened (hostname={hostname}, peer={addr})");

    Ok(Json(BeginResponse {
        login_url: format!("{base}/login/cli/{session_id}"),
        done_url: format!("{base}/registry/npm/-/v1/done/{session_id}"),
    }))
}

async fn poll_done(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> Result<axum::response::Response, AppError> {
    let now = chrono::Utc::now().timestamp();
    let session_id_db = session_id.clone();

    // Single round-trip: load row + flip-and-clear if authorised. Doing the
    // clear in the same transaction means even a duplicated poll only ever
    // returns the plaintext once.
    let state_cl = state.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<PollOutcome, AppError> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        let tx = db
            .unchecked_transaction()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;

        let row: Option<(String, i64, Option<String>)> = tx
            .query_row(
                "SELECT status, expires_at, token_plaintext_enc
                 FROM npm_login_sessions WHERE session_id = ?1",
                [&session_id_db],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("load session: {e}")))?;

        let Some((status, expires_at, encrypted)) = row else {
            return Ok(PollOutcome::NotFound);
        };
        if now > expires_at {
            // Lazy expiry — mark and report. Saves a background sweep round trip.
            let _ = tx.execute(
                "UPDATE npm_login_sessions SET status = 'expired',
                                              token_plaintext_enc = NULL
                 WHERE session_id = ?1",
                [&session_id_db],
            );
            tx.commit()
                .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;
            return Ok(PollOutcome::Expired);
        }
        if status == "authorized" {
            let plaintext = match encrypted {
                Some(blob) => crypto::decrypt(&blob, &crypto::get_secret_key())
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt token: {e}")))?,
                None => {
                    // Authorised but already consumed — treat like expired so
                    // the CLI stops polling cleanly.
                    return Ok(PollOutcome::Expired);
                }
            };
            tx.execute(
                "UPDATE npm_login_sessions SET token_plaintext_enc = NULL
                 WHERE session_id = ?1",
                [&session_id_db],
            )
            .map_err(|e| AppError::Internal(anyhow::anyhow!("clear plaintext: {e}")))?;
            tx.commit()
                .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;
            Ok(PollOutcome::Authorized(plaintext))
        } else {
            // status == 'pending' (or any other future state) — leave as-is.
            Ok(PollOutcome::Pending)
        }
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))??;

    Ok(match outcome {
        PollOutcome::Pending => StatusCode::ACCEPTED.into_response(),
        PollOutcome::Authorized(token) => Json(PollAuthorized { token }).into_response(),
        PollOutcome::Expired => (StatusCode::GONE, "session expired").into_response(),
        PollOutcome::NotFound => StatusCode::NOT_FOUND.into_response(),
    })
}

enum PollOutcome {
    Pending,
    Authorized(String),
    Expired,
    NotFound,
}

// ----- panel-facing endpoints ------------------------------------------------

async fn session_status(
    State(state): State<SharedState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionStatus>, AppError> {
    let session_id_db = session_id.clone();
    let state_cl = state.clone();
    let row = tokio::task::spawn_blocking(move || -> Result<Option<SessionStatus>, AppError> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        db.query_row(
            "SELECT session_id, hostname, status, peer_ip, user_agent, expires_at
                 FROM npm_login_sessions WHERE session_id = ?1",
            [&session_id_db],
            |row| {
                Ok(SessionStatus {
                    session_id: row.get(0)?,
                    hostname: row.get(1)?,
                    status: row.get(2)?,
                    peer_ip: row.get(3)?,
                    user_agent: row.get(4)?,
                    expires_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("load session: {e}")))
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))??;

    row.map(Json)
        .ok_or_else(|| AppError::NotFound(format!("login session {session_id}")))
}

async fn authorize_session(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path(session_id): Path<String>,
    Json(body): Json<AuthorizeBody>,
) -> Result<Json<AuthorizeResponse>, AppError> {
    let now = chrono::Utc::now().timestamp();

    // Generate token + name *outside* the blocking closure so the secret
    // material is built once (and dropped) regardless of which branch the
    // transaction takes.
    let issued = api_token::generate();
    let plaintext = issued.plaintext.clone();
    let encrypted = crypto::encrypt(&plaintext, &crypto::get_secret_key())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt token: {e}")))?;

    let session_id_db = session_id.clone();
    let user_id = user.id.clone();
    let username = user.username.clone();
    let state_cl = state.clone();
    let token_name = body
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| format!("npm web login ({session_id_db})"));

    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;

        let row: Option<(String, i64)> = db
            .query_row(
                "SELECT status, expires_at FROM npm_login_sessions WHERE session_id = ?1",
                [&session_id_db],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("load session: {e}")))?;

        let Some((status, expires_at)) = row else {
            return Err(AppError::NotFound(format!("login session {session_id_db}")));
        };
        if now > expires_at {
            return Err(AppError::BadRequest("session expired".into()));
        }
        if status != "pending" {
            return Err(AppError::Conflict(format!("session is already {status}")));
        }

        let tx = db
            .unchecked_transaction()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;

        api_token::store(&tx, &issued, &user_id, &token_name)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("store token: {e}")))?;

        tx.execute(
            "UPDATE npm_login_sessions
             SET status = 'authorized',
                 token_id = ?1,
                 token_plaintext_enc = ?2
             WHERE session_id = ?3",
            rusqlite::params![&issued.id, &encrypted, &session_id_db],
        )
        .map_err(|e| AppError::Internal(anyhow::anyhow!("link token to session: {e}")))?;

        tx.commit()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;
        Ok(())
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))??;

    tracing::info!("npm web-login: session {session_id} authorised by {username}");

    Ok(Json(AuthorizeResponse {
        ok: true,
        token: plaintext,
    }))
}

// ----- maintenance ------------------------------------------------------------

/// Drop expired/consumed sessions on a slow timer. Cheap enough to run every
/// few minutes; mostly there to keep the table from accruing rows forever.
pub fn spawn_sweep_task(state: SharedState) {
    tokio::spawn(async move {
        // Sweep on startup then every 5 minutes.
        loop {
            if let Err(e) = sweep_once(&state).await {
                tracing::warn!("npm web-login sweep failed: {e:#}");
            }
            sleep(Duration::from_secs(300)).await;
        }
    });
}

async fn sweep_once(state: &SharedState) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let state_cl = state.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let removed = db.execute(
            "DELETE FROM npm_login_sessions WHERE expires_at < ?1",
            [now],
        )?;
        if removed > 0 {
            tracing::info!("npm web-login: swept {removed} expired session(s)");
        }
        Ok(())
    })
    .await??;
    Ok(())
}

/// Detect the public scheme/host so the CLI receives URLs that resolve back
/// to this Pier instance. Mirrors `api::npm::public_base_url` but kept local
/// to avoid a cross-module pub fn (the npm module's helper is private).
fn public_base_url(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "http".to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    format!("{scheme}://{host}")
}
