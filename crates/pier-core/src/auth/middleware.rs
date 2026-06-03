use axum::extract::{OriginalUri, Request, State};
use axum::http::header::{AUTHORIZATION, COOKIE, SET_COOKIE, USER_AGENT};
use axum::http::HeaderValue;
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;

use std::time::Duration;
use tokio::time::sleep;

use crate::auth::api_token;
use crate::auth::cookie::{build_session_cookie, clear_session_cookies};
use crate::auth::password;
use crate::auth::rbac::GlobalRole;
use crate::error::AppError;
use crate::state::SharedState;

/// Header used by a remote Pier core to authenticate its cross-core API calls.
/// Must match a row in the `peer_grants` table with `is_active = 1`.
pub const PEER_TOKEN_HEADER: &str = "X-Pier-Peer-Token";

/// Whether a path belongs to the JSON API surface (vs the HTML UI). Affects
/// the failure mode of unauthenticated requests: API → 401, UI → 303 to
/// `/login`. Pulled out of the middleware so it can be unit-tested without
/// constructing a full `Request`.
fn is_api_path(full_path: &str) -> bool {
    full_path.starts_with("/api/") || full_path.starts_with("/registry/")
}

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

/// Extract every value of the session cookie from a raw `Cookie:` header.
///
/// A site can legitimately carry MORE THAN ONE cookie with the same name —
/// RFC 6265 allows it when they differ by `Domain` or `Path`. A browser sends
/// them all in a single header, e.g. `pier_session=dead; other=x; pier_session=live`.
/// Returning every distinct non-empty value (in header order) lets the caller
/// authenticate on whichever one maps to a live session instead of being held
/// hostage by header order. Picking only the first entry let a stale shadow
/// cookie (scoped to a parent `Domain` or an old `Path`, which our clear-cookie
/// backstop can't evict) mask the live one — bouncing the operator to `/login`
/// until they manually wiped all cookies.
fn session_cookie_values<'a>(cookie_header: &'a str, name: &str) -> Vec<&'a str> {
    let prefix = format!("{name}=");
    let mut out = Vec::new();
    for part in cookie_header.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&prefix) {
            if !v.is_empty() && !out.contains(&v) {
                out.push(v);
            }
        }
    }
    out
}

/// Coarse device signature for a `User-Agent`: drop version numbers (digits and
/// dots) and lower-case the rest, so a routine browser auto-update (Chrome 120 →
/// 121) keeps the same fingerprint while a real browser/OS swap changes it. Used
/// to bind a session to the device it was created on.
fn ua_fingerprint(ua: &str) -> String {
    let mut out = String::with_capacity(ua.len());
    let mut prev_space = false;
    for c in ua.chars() {
        if c.is_ascii_digit() || c == '.' {
            continue;
        }
        if c.is_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        out.push(c.to_ascii_lowercase());
        prev_space = false;
    }
    out.trim_end().to_string()
}

/// Build a comma-separated `?,?,…` placeholder list of length `n` for an SQL
/// `IN (...)` clause.
fn sql_in_placeholders(n: usize) -> String {
    (0..n).map(|_| "?").collect::<Vec<_>>().join(",")
}

/// User info extracted from a valid session, stored in request extensions.
#[derive(Clone, Debug)]
pub struct AuthUser {
    pub id: String,
    pub username: String,
    /// Legacy free-form role string — kept populated for back-compat with the
    /// pre-RBAC `user.role == "admin"` call-sites still in flight. New code
    /// should reach for [`Self::global_role`] instead.
    pub role: String,
    /// Typed system role used by RBAC guards and policy checks. Populated
    /// from `users.global_role` (or synthesised for peer-token requests).
    pub global_role: GlobalRole,
    /// ID of the session cookie this request authenticated with. Lets handlers
    /// distinguish "current session" from other active sessions (e.g. to avoid
    /// revoking the caller's own session in /account/sessions).
    ///
    /// Empty string for peer-token authenticated requests (no session).
    pub session_id: String,
    /// True if this request was authenticated via the `X-Pier-Peer-Token`
    /// federation header rather than a local user credential. Peer requests
    /// bypass project-membership checks but cannot mutate user records.
    pub is_peer: bool,
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
    // Inside a `nest()`ed router axum rewrites `req.uri()` to the *stripped*
    // path (e.g. `/registry/npm/-/whoami` becomes `/-/whoami` once handed to
    // the npm router). The `OriginalUri` extension preserves the full URL —
    // we need it to classify `/api/...` and `/registry/...` correctly, because
    // npm clients can't follow the HTML redirect we'd otherwise emit for
    // "UI" paths.
    let full_path: String = req
        .extensions()
        .get::<OriginalUri>()
        .map(|o| o.0.path().to_string())
        .unwrap_or_else(|| path.to_string());
    let is_api = is_api_path(&full_path);

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
                // Peers act with Admin-level reach for resource operations
                // but are filtered out of user-management routes by the
                // `is_peer` flag in `rbac::policy::can`.
                global_role: GlobalRole::Admin,
                session_id: String::new(),
                is_peer: true,
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
    let basic_opt = if full_path.starts_with("/registry/") {
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
                let row: Option<(String, String, String, String, String)> = db
                    .query_row(
                        "SELECT id, username, password, role, global_role FROM users
                         WHERE (username = ?1 OR email = ?1) AND is_active = 1",
                        [&basic_user],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                                row.get::<_, String>(4)?,
                            ))
                        },
                    )
                    .ok();
                let Some((id, username, hash, role, global_role)) = row else {
                    return Ok(None);
                };
                if password::verify_password(&basic_pass, &hash)? {
                    Ok(Some(AuthUser {
                        id,
                        username,
                        role,
                        global_role: GlobalRole::parse(&global_role).unwrap_or(GlobalRole::User),
                        session_id: String::new(),
                        is_peer: false,
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

    // 3. Fall back to session cookie(s).
    //
    // The browser may present several `pier_session` cookies at once (a live
    // one plus stale shadows scoped to a parent `Domain`/old `Path`). They all
    // arrive in this one header, so we collect EVERY value and try each against
    // the DB — authenticating on the first that maps to a live session rather
    // than trusting header order. See `session_cookie_values` for why.
    let cookie_header = req
        .headers()
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // Collect to owned strings so no immutable borrow of `req` lingers into the
    // `req.extensions_mut()` call below.
    let candidates: Vec<String> =
        session_cookie_values(cookie_header, &state.config.session_cookie)
            .into_iter()
            .map(|s| s.to_string())
            .collect();

    if candidates.is_empty() {
        return if is_api {
            Err(AppError::Unauthorized)
        } else {
            Ok(Redirect::to(&login_redirect_target(&req)).into_response())
        };
    }

    // Evaluated alongside the idle/expiry check below: the absolute lifetime cap
    // (a session may never live past `created_at + abs_max`, even with sliding)
    // and the device fingerprint of THIS request (a session is bound to the
    // browser/OS it was created on). Computed up front so no borrow of `req`
    // lingers into the `req.extensions_mut()` call later.
    let abs_max = state.config.session_abs_max_hours.max(1) as i64;
    let req_ua_fp = ua_fingerprint(
        req.headers()
            .get(USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    );

    // Look up each candidate session in the DB; the first live one wins.
    let (auth_user, session_refreshed) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;

        let mut found: Option<AuthUser> = None;
        for sid in &candidates {
            let result = db.query_row(
                "SELECT u.id, u.username, u.role, u.global_role, s.user_agent
                 FROM sessions s
                 JOIN users u ON s.user_id = u.id
                 WHERE s.id = ?1
                   AND s.expires_at > datetime('now')
                   AND s.created_at > datetime('now', '-' || ?2 || ' hours')
                   AND u.is_active = 1",
                rusqlite::params![sid, abs_max],
                |row| {
                    let global_role_str: String = row.get(3)?;
                    let stored_ua: Option<String> = row.get(4)?;
                    Ok((
                        AuthUser {
                            id: row.get(0)?,
                            username: row.get(1)?,
                            role: row.get(2)?,
                            global_role: GlobalRole::parse(&global_role_str)
                                .unwrap_or(GlobalRole::User),
                            session_id: sid.clone(),
                            is_peer: false,
                        },
                        stored_ua,
                    ))
                },
            );
            if let Ok((user, stored_ua)) = result {
                // Device binding: if the request's UA fingerprint no longer
                // matches the one the session was minted with, the cookie is
                // being replayed from a different browser/OS — revoke it rather
                // than honour it. Version bumps are stripped by `ua_fingerprint`,
                // so routine browser updates don't trip this.
                let stored_fp = stored_ua.as_deref().map(ua_fingerprint).unwrap_or_default();
                if !req_ua_fp.is_empty() && !stored_fp.is_empty() && stored_fp != req_ua_fp {
                    let _ = db.execute("DELETE FROM sessions WHERE id = ?1", [sid]);
                    tracing::info!(
                        path = %full_path,
                        "auth: session device/User-Agent changed; revoking session"
                    );
                    break; // `found` stays None → clear cookie + redirect below
                }
                found = Some(user);
                break;
            }
        }

        match found {
            Some(user) => {
                // Sliding expiration: once a session is past the halfway point
                // of its TTL, roll `expires_at` forward so an actively-used
                // session never expires mid-flight. The `WHERE` guard bounds
                // this to at most one write per ~half-TTL window per session,
                // not one per request, and the `created_at` guard refuses to
                // slide a session past its absolute lifetime. A non-zero row
                // count means we extended it — so we refresh the cookie too.
                let ttl = state.config.session_ttl_hours.max(1) as i64;
                let half_ttl_min = (ttl * 60) / 2;
                let extended = db
                    .execute(
                        "UPDATE sessions \
                         SET expires_at = datetime('now', '+' || ?2 || ' hours') \
                         WHERE id = ?1 \
                           AND expires_at < datetime('now', '+' || ?3 || ' minutes') \
                           AND created_at > datetime('now', '-' || ?4 || ' hours')",
                        rusqlite::params![user.session_id, ttl, half_ttl_min, abs_max],
                    )
                    .unwrap_or(0);
                (user, extended > 0)
            }
            None => {
                // Cookie(s) were sent but none map to a live session. Log it so
                // the next occurrence is self-explaining in journalctl, then
                // tell the browser to drop the dead cookie(s) — otherwise the
                // operator can get stuck in a /login bounce that only manual
                // cookie-clearing fixes. We log only an 8-char prefix of the
                // session id, never the full secret.
                let first_prefix: String = candidates
                    .first()
                    .map(|s| s.chars().take(8).collect())
                    .unwrap_or_default();
                tracing::info!(
                    candidate_cookies = candidates.len(),
                    first_id_prefix = %first_prefix,
                    path = %full_path,
                    is_api,
                    "auth: session cookie(s) present but none mapped to a live session; clearing and redirecting to login"
                );
                // Reap the dead value(s) we were just shown so abandoned/expired
                // rows don't linger. Value-safe: only deletes rows that ACTUALLY
                // failed validation (expired OR past the absolute lifetime); a
                // still-live session value riding in the same header for another
                // logged-in user is excluded by both predicates and survives.
                let sql = format!(
                    "DELETE FROM sessions WHERE id IN ({}) \
                     AND (expires_at <= datetime('now') \
                          OR created_at <= datetime('now', '-' || ? || ' hours'))",
                    sql_in_placeholders(candidates.len())
                );
                let mut params: Vec<&dyn rusqlite::ToSql> = candidates
                    .iter()
                    .map(|s| s as &dyn rusqlite::ToSql)
                    .collect();
                params.push(&abs_max);
                let reaped = db.execute(&sql, params.as_slice()).unwrap_or(0);
                if reaped > 0 {
                    tracing::info!(reaped, "auth: deleted dead session row(s) on detection");
                }
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

    // Inject AuthUser, run the handler, then — if we just slid the session
    // forward — re-issue the cookie so its client-side Max-Age tracks the
    // server-side expiry and the operator stays logged in while actively using
    // the panel.
    let refreshed_sid = if session_refreshed {
        Some(auth_user.session_id.clone())
    } else {
        None
    };
    req.extensions_mut().insert(auth_user);
    let mut response = next.run(req).await;
    if let Some(sid) = refreshed_sid {
        let ttl = state.config.session_ttl_hours.max(1) as i64;
        if let Ok(v) = HeaderValue::from_str(&build_session_cookie(&state, &sid, ttl * 3600)) {
            response.headers_mut().append(SET_COOKIE, v);
        }
    }
    Ok(response)
}

/// Background sweep that deletes session rows past their idle expiry or absolute
/// lifetime. The opportunistic reap in `require_auth` only removes cookies that
/// are re-presented; this catches abandoned ones (browser closed, cookie
/// cleared) so the table doesn't grow without bound. Mirrors
/// `api::npm_web_login::spawn_sweep_task`.
pub fn spawn_session_gc_task(state: SharedState) {
    tokio::spawn(async move {
        loop {
            if let Err(e) = session_gc_once(&state).await {
                tracing::warn!("session GC sweep failed: {e:#}");
            }
            sleep(Duration::from_secs(900)).await;
        }
    });
}

async fn session_gc_once(state: &SharedState) -> anyhow::Result<()> {
    // Operator kill-switch (default on), read fresh each tick so it can be
    // toggled without a restart — same convention as the rotation/audit jobs.
    let enabled = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            ["session.gc_enabled"],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true)
    };
    if !enabled {
        return Ok(());
    }

    let abs_max = state.config.session_abs_max_hours.max(1) as i64;
    let state_cl = state.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let removed = db.execute(
            "DELETE FROM sessions \
             WHERE expires_at <= datetime('now') \
                OR created_at <= datetime('now', '-' || ?1 || ' hours')",
            rusqlite::params![abs_max],
        )?;
        if removed > 0 {
            tracing::info!("session GC: swept {removed} dead session row(s)");
        }
        Ok(())
    })
    .await??;
    Ok(())
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
    fn is_api_path_matches_api_and_registry_only() {
        assert!(is_api_path("/api/v1/users"));
        assert!(is_api_path("/api/v1/registry/packages/foo"));
        assert!(is_api_path("/registry/npm/-/whoami"));
        assert!(is_api_path("/registry/npm/@scope/pkg"));
        // UI paths must NOT be classified as API — otherwise unauth requests
        // would 401 instead of redirecting to /login.
        assert!(!is_api_path("/login"));
        assert!(!is_api_path("/packages"));
        assert!(!is_api_path("/login/cli/abc123"));
        assert!(!is_api_path("/"));
        // Strict prefix — `/registryctl` is not the embedded registry.
        assert!(!is_api_path("/registryctl"));
        assert!(!is_api_path("/apiary"));
    }

    #[test]
    fn session_cookie_values_extracts_single() {
        assert_eq!(
            session_cookie_values("pier_session=abc", "pier_session"),
            vec!["abc"]
        );
        // Surrounded by other cookies, with whitespace after the separator.
        assert_eq!(
            session_cookie_values("theme=dark; pier_session=abc; foo=bar", "pier_session"),
            vec!["abc"]
        );
    }

    #[test]
    fn session_cookie_values_returns_all_duplicates_in_order() {
        // The regression case: a dead shadow cookie ahead of the live one.
        // Both must be returned so the caller can try each, instead of being
        // stuck on the first (dead) value and bouncing to /login.
        assert_eq!(
            session_cookie_values(
                "pier_session=dead; other=x; pier_session=live",
                "pier_session"
            ),
            vec!["dead", "live"]
        );
    }

    #[test]
    fn session_cookie_values_dedupes_and_skips_empty() {
        assert_eq!(
            session_cookie_values(
                "pier_session=a; pier_session=; pier_session=a",
                "pier_session"
            ),
            vec!["a"]
        );
    }

    #[test]
    fn session_cookie_values_ignores_other_names_and_empty_header() {
        assert!(session_cookie_values("session=abc; csrf=xyz", "pier_session").is_empty());
        assert!(session_cookie_values("", "pier_session").is_empty());
        // Honours a custom cookie name.
        assert_eq!(session_cookie_values("custom=abc", "custom"), vec!["abc"]);
    }

    #[test]
    fn ua_fingerprint_ignores_version_bumps() {
        let chrome120 = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";
        let chrome121 = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/121.0.6167.85 Safari/537.36";
        // A version bump on the same browser/OS keeps the same fingerprint, so a
        // routine auto-update does NOT revoke the session.
        assert_eq!(ua_fingerprint(chrome120), ua_fingerprint(chrome121));
    }

    #[test]
    fn ua_fingerprint_distinguishes_browser_and_os() {
        let chrome_win = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120.0.0.0 Safari/537.36";
        let firefox_win = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Gecko/20100101 Firefox/121.0";
        let chrome_linux = "Mozilla/5.0 (X11; Linux x86_64) Chrome/120.0.0.0 Safari/537.36";
        assert_ne!(ua_fingerprint(chrome_win), ua_fingerprint(firefox_win));
        assert_ne!(ua_fingerprint(chrome_win), ua_fingerprint(chrome_linux));
        // Empty stays empty — the "unknown UA → don't revoke" sentinel.
        assert!(ua_fingerprint("").is_empty());
    }

    #[test]
    fn sql_in_placeholders_shape() {
        assert_eq!(sql_in_placeholders(1), "?");
        assert_eq!(sql_in_placeholders(2), "?,?");
        assert_eq!(sql_in_placeholders(3), "?,?,?");
        assert_eq!(sql_in_placeholders(0), "");
    }

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
