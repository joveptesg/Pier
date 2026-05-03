use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use crate::auth::api_token;
use crate::error::AppError;
use crate::state::SharedState;

/// Header used by a remote Pier core to authenticate its cross-core API calls.
/// Must match a row in the `peer_grants` table with `is_active = 1`.
pub const PEER_TOKEN_HEADER: &str = "X-Pier-Peer-Token";

/// Extract the bearer value from an `Authorization: Bearer …` header.
fn parse_bearer(header: &str) -> Option<&str> {
    let trimmed = header.trim();
    let stripped = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))?;
    let value = stripped.trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

/// User info extracted from a valid session, stored in request extensions.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
    pub role: String,
    /// ID of the session cookie this request authenticated with. Lets handlers
    /// distinguish "current session" from other active sessions (e.g. to avoid
    /// revoking the caller's own session in /account/sessions).
    ///
    /// Empty string for peer-token authenticated requests (no session).
    pub session_id: String,
}

/// Middleware that checks for a valid session cookie OR a peer-core bearer token.
/// For protected routes: injects AuthUser into request extensions.
/// If no valid auth: redirects to /login for UI routes, returns 401 for API routes.
pub async fn require_auth(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let path = req.uri().path();
    // `/registry/...` is the embedded npm-compatible registry. Treat it as
    // API for auth purposes — 401s, not redirects to /login — because npm
    // clients can't follow HTML redirects.
    let is_api = path.starts_with("/api/") || path.starts_with("/registry/");

    // 1. Check for peer-core token first — only accepted on API routes.
    //    This lets another pier-core act as an admin on this instance.
    let peer_token_opt = if is_api {
        req.headers()
            .get(PEER_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    } else {
        None
    };
    if let Some(token) = peer_token_opt {
        let peer_grant = {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
            db.query_row(
                "SELECT id, name FROM peer_grants WHERE token = ?1 AND is_active = 1",
                [&token],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok()
        };
        if let Some((grant_id, grant_name)) = peer_grant {
            // Update last_used_at (best-effort; ignore errors).
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
                let _ = db.execute(
                    "UPDATE peer_grants SET last_used_at = datetime('now') WHERE id = ?1",
                    [&grant_id],
                );
            }
            req.extensions_mut().insert(AuthUser {
                id: format!("peer:{grant_id}"),
                username: format!("peer:{grant_name}"),
                role: "peer_admin".to_string(),
                session_id: String::new(),
            });
            return Ok(next.run(req).await);
        }
        // Token present but invalid — fail fast rather than fall through to session auth.
        return Err(AppError::Unauthorized);
    }

    // 2. Check for an `Authorization: Bearer <api-token>` header.
    //    Used by npm CLI, CI runners, and other clients that can't carry a
    //    session cookie. Tokens are validated against `api_tokens` (sha256 hash).
    let bearer_opt = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(parse_bearer)
        .map(|s| s.to_string());
    if let Some(plaintext) = bearer_opt {
        let lookup_result = {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
            api_token::lookup(&db, &plaintext)
        };
        match lookup_result {
            Ok(Some(user)) => {
                req.extensions_mut().insert(user);
                return Ok(next.run(req).await);
            }
            Ok(None) => {
                // Bearer header was present but didn't match a live token —
                // don't fall through to session auth, that's a token-bearing
                // client (npm/CI) and they expect a 401.
                return Err(AppError::Unauthorized);
            }
            Err(e) => {
                tracing::error!("api_token lookup failed: {e}");
                return Err(AppError::Unauthorized);
            }
        }
    }

    // 3. Fall back to session cookie.
    let session_id = req
        .headers()
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|c| {
                let c = c.trim();
                c.strip_prefix(&format!("{}=", state.config.session_cookie))
            })
        });

    let session_id = match session_id {
        Some(id) if !id.is_empty() => id.to_string(),
        _ => {
            return if is_api {
                Err(AppError::Unauthorized)
            } else {
                Ok(Redirect::to("/login").into_response())
            };
        }
    };

    // Look up session and user in DB
    let auth_user = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;

        let sid = session_id.clone();
        let result = db.query_row(
            "SELECT u.id, u.username, u.role
             FROM sessions s
             JOIN users u ON s.user_id = u.id
             WHERE s.id = ?1
               AND s.expires_at > datetime('now')
               AND u.is_active = 1",
            [&session_id],
            |row| {
                Ok(AuthUser {
                    id: row.get(0)?,
                    username: row.get(1)?,
                    role: row.get(2)?,
                    session_id: sid.clone(),
                })
            },
        );

        match result {
            Ok(user) => user,
            Err(_) => {
                return if is_api {
                    Err(AppError::Unauthorized)
                } else {
                    Ok(Redirect::to("/login").into_response())
                };
            }
        }
    };

    // Inject AuthUser into request extensions
    req.extensions_mut().insert(auth_user);
    Ok(next.run(req).await)
}
