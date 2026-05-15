use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;

use crate::auth::api_token;
use crate::auth::cookie::clear_session_cookies;
use crate::auth::password;
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

/// Extract (user, pass) from an `Authorization: Basic base64(user:pass)` header.
/// Used by yarn classic when it stores `_auth=<base64>` in `.npmrc` instead of
/// `_authToken`. Returns None on any parse failure.
fn parse_basic(header: &str) -> Option<(String, String)> {
    let trimmed = header.trim();
    let stripped = trimmed
        .strip_prefix("Basic ")
        .or_else(|| trimmed.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(stripped.trim())
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    if user.is_empty() {
        return None;
    }
    Some((user.to_string(), pass.to_string()))
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

    // 2b. `Authorization: Basic base64(user:pass)` — only honoured on the
    //     embedded npm registry routes, where yarn classic still uses `_auth`
    //     in `.npmrc`. We intentionally do NOT accept Basic on the rest of the
    //     API: there's no use case beyond the npm CLI, and refusing it
    //     elsewhere keeps the attack surface narrow.
    let basic_opt = if path.starts_with("/registry/") {
        req.headers()
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_basic)
    } else {
        None
    };
    if let Some((basic_user, basic_pass)) = basic_opt {
        // bcrypt verify is CPU-heavy (~100ms on a small VPS) — run on the
        // blocking pool so the async runtime stays free for other in-flight
        // installs.
        let state_cl = state.clone();
        let lookup =
            tokio::task::spawn_blocking(move || -> Result<Option<AuthUser>, anyhow::Error> {
                let db = state_cl
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
                let row: Option<(String, String, String, String)> = db
                    .query_row(
                        "SELECT id, username, password, role FROM users
                         WHERE (username = ?1 OR email = ?1) AND is_active = 1",
                        [&basic_user],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .ok();
                let Some((id, username, hash, role)) = row else {
                    return Ok(None);
                };
                if password::verify_password(&basic_pass, &hash)? {
                    Ok(Some(AuthUser {
                        id,
                        username,
                        role,
                        session_id: String::new(),
                    }))
                } else {
                    Ok(None)
                }
            })
            .await
            .map_err(|e| anyhow::anyhow!("blocking task: {e}"))?;
        match lookup {
            Ok(Some(user)) => {
                req.extensions_mut().insert(user);
                return Ok(next.run(req).await);
            }
            Ok(None) => {
                // Header present but credentials invalid — same policy as
                // Bearer above: don't fall through, return 401 so the npm
                // client can re-prompt.
                return Err(AppError::Unauthorized);
            }
            Err(e) => {
                tracing::error!("basic auth lookup failed: {e}");
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
                Ok(Redirect::to(&login_redirect_target(&req)).into_response())
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
                // Cookie was sent but no live session matches it. Tell the
                // browser to drop the dead cookie so the next request comes
                // in clean — otherwise the operator can get stuck in a
                // /login bounce that only manual cookie-clearing fixes.
                let mut response = if is_api {
                    AppError::Unauthorized.into_response()
                } else {
                    Redirect::to(&login_redirect_target(&req)).into_response()
                };
                attach_clear_session_cookies(&mut response, &state);
                return Ok(response);
            }
        }
    };

    // Inject AuthUser into request extensions
    req.extensions_mut().insert(auth_user);
    Ok(next.run(req).await)
}

/// Append `Set-Cookie` headers that delete the session cookie on the client.
/// Used when the request brought a `pier_session` cookie that no longer maps
/// to a live session row — without this, browsers happily keep replaying the
/// dead cookie on every redirect to /login.
fn attach_clear_session_cookies(response: &mut Response, state: &SharedState) {
    for raw in clear_session_cookies(state) {
        match HeaderValue::from_str(&raw) {
            Ok(v) => {
                response.headers_mut().append(SET_COOKIE, v);
            }
            Err(e) => {
                // Cookie name is operator-controlled (`PIER_SESSION_COOKIE`).
                // Anything that fails HeaderValue parsing is a misconfig, not
                // a runtime concern — log and move on.
                tracing::warn!("clear-session-cookie header rejected: {e}");
            }
        }
    }
}

/// Build the redirect target for an unauthenticated UI request. Plain `/login`
/// unless we can safely thread the original path through `?return_to=…`, in
/// which case `login.html` will bounce the user back after login.
///
/// Safety: only encode internal paths (`/foo/bar`) to avoid an open-redirect
/// gadget. Anything that doesn't start with `/`, or contains `\\` / `://`, is
/// dropped on the floor.
fn login_redirect_target(req: &Request) -> String {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("");
    login_redirect_for_path(path_and_query)
}

fn login_redirect_for_path(path_and_query: &str) -> String {
    // Reject anything that isn't an internal path. `//host/...` is a
    // schema-relative URL — browsers expand it to a fully qualified URL on
    // the current scheme, so treating it as "internal" would let an attacker
    // redirect a victim off-domain after login.
    if !path_and_query.starts_with('/')
        || path_and_query.starts_with("//")
        || path_and_query.contains("://")
        || path_and_query.contains('\\')
    {
        return "/login".to_string();
    }
    // Don't loop back if we're already on /login.
    if path_and_query == "/login" || path_and_query.starts_with("/login?") {
        return "/login".to_string();
    }
    let encoded = urlencoding::encode(path_and_query);
    format!("/login?return_to={encoded}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        assert_eq!(parse_bearer("Bearer abc"), Some("abc"));
        assert_eq!(parse_bearer("bearer XYZ"), Some("XYZ"));
        assert_eq!(parse_bearer("  Bearer  abc  "), Some("abc"));
        assert_eq!(parse_bearer("Basic xxx"), None);
        assert_eq!(parse_bearer("Bearer "), None);
        assert_eq!(parse_bearer(""), None);
    }

    #[test]
    fn basic_parsing_decodes_credentials() {
        let header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("alice:hunter2")
        );
        assert_eq!(
            parse_basic(&header),
            Some(("alice".to_string(), "hunter2".to_string()))
        );
    }

    #[test]
    fn basic_parsing_accepts_empty_password() {
        let header = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("alice:")
        );
        assert_eq!(
            parse_basic(&header),
            Some(("alice".to_string(), "".to_string()))
        );
    }

    #[test]
    fn basic_parsing_rejects_garbage() {
        assert_eq!(parse_basic("Basic not-base64"), None);
        assert_eq!(parse_basic("Basic"), None);
        assert_eq!(parse_basic("Bearer abc"), None);
        // Missing colon — not a credentials string.
        let b64 = base64::engine::general_purpose::STANDARD.encode("nocolonhere");
        assert_eq!(parse_basic(&format!("Basic {b64}")), None);
        // Empty username.
        let b64 = base64::engine::general_purpose::STANDARD.encode(":lonelypass");
        assert_eq!(parse_basic(&format!("Basic {b64}")), None);
    }

    #[test]
    fn login_redirect_encodes_safe_paths() {
        assert_eq!(
            login_redirect_for_path("/login/cli/abc123"),
            "/login?return_to=%2Flogin%2Fcli%2Fabc123"
        );
        assert_eq!(
            login_redirect_for_path("/packages?tab=tokens"),
            "/login?return_to=%2Fpackages%3Ftab%3Dtokens"
        );
    }

    #[test]
    fn login_redirect_refuses_external_or_recursive() {
        // Open-redirect attempt.
        assert_eq!(
            login_redirect_for_path("https://evil.example/path"),
            "/login"
        );
        // Schema-relative URL (also an open-redirect risk).
        assert_eq!(login_redirect_for_path("//evil.example/path"), "/login");
        // Already on /login — don't loop.
        assert_eq!(login_redirect_for_path("/login"), "/login");
        assert_eq!(login_redirect_for_path("/login?foo=bar"), "/login");
        // Empty / non-absolute.
        assert_eq!(login_redirect_for_path(""), "/login");
        assert_eq!(login_redirect_for_path("login"), "/login");
    }
}
