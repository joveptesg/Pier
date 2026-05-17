//! Federation-token authentication for the write-federation surface
//! (`/api/v1/agent/*` on a peer pier-core).
//!
//! A federation token is minted on the **peer** side, copied by the
//! operator into the **primary** core's UI, and thereafter presented in
//! the `X-Pier-Federation` header on every primary→peer write call.
//!
//! Why this is its own header and table rather than reusing `peer_grants`
//! / `X-Pier-Peer-Token`:
//! - peer_grants carries the plaintext token in the DB (legacy). The new
//!   surface gets to start hash-only.
//! - peer_grants confers broad UI-equivalent access on a peer core. A
//!   federation token must be scoped strictly to `/api/v1/agent/*` so a
//!   leaked token can't open admin pages. Separate headers let the
//!   middleware refuse cross-use trivially.
//! - Ownership tracking on managed resources (`services.owner_server_id`)
//!   needs to know **which** token wrote the row. Putting that in
//!   `req.extensions()` here makes the downstream handlers trivial.

// Most items here are referenced only by /api/v1/agent/* (2.3) and the
// token-management handlers (2.5). Until those land the symbols look
// unused — re-evaluate this allow once those phases are committed.
#![allow(dead_code)]

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::Response;
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::state::SharedState;

/// Header name expected by the peer's `/api/v1/agent/*` router.
pub const FEDERATION_HEADER: &str = "X-Pier-Federation";

/// Prefix attached to every minted federation token. Lets operators
/// recognise the kind of credential at a glance and lets future code
/// refuse, say, a `pier_srv_…` agent token presented in
/// `X-Pier-Federation`.
pub const FEDERATION_PREFIX: &str = "pier_fed_";

/// Number of characters of the token (incl. prefix) shown in the UI as
/// a fingerprint. Matches [`super::server_token::DISPLAY_PREFIX_LEN`].
pub const DISPLAY_PREFIX_LEN: usize = 16;

/// A freshly minted federation token. `plaintext` is returned to the
/// caller exactly once (the operator copies it from the UI), then the
/// peer keeps only the SHA-256 hash and the visible prefix.
pub struct IssuedFederationToken {
    pub plaintext: String,
    pub prefix: String,
}

/// Generate a new federation token. 24 random bytes hex-encoded ≈ 48
/// hex chars after the prefix — same entropy budget as agent tokens.
pub fn generate() -> IssuedFederationToken {
    let bytes: [u8; 24] = rand::random();
    let plaintext = format!("{FEDERATION_PREFIX}{}", hex::encode(bytes));
    let prefix = plaintext.chars().take(DISPLAY_PREFIX_LEN).collect();
    IssuedFederationToken { plaintext, prefix }
}

/// sha256-hex of a plaintext federation token. Same encoding as
/// [`super::server_token::hash`] and [`super::api_token::hash`], kept
/// as a separate function so callers cannot accidentally validate one
/// kind of token against another's hash column.
pub fn hash(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

/// Context attached to every request that succeeds federation auth.
/// Downstream handlers retrieve this from `req.extensions()` to know
/// which primary they're acting on behalf of (so `owner_server_id` on
/// any rows they mutate can be set to `token_id`).
#[derive(Debug, Clone)]
pub struct FederationContext {
    /// Primary key of the row in `federation_tokens`. Used as the value
    /// of `services.owner_server_id` / `projects.owner_server_id` when
    /// a managed resource is created or claimed.
    pub token_id: String,
    /// Operator-chosen label of the primary that holds this token.
    /// Surfaced in the peer UI ("managed by vps1-master") without
    /// having to re-query the federation_tokens table.
    pub primary_label: String,
}

/// Axum middleware. Refuses the request with 401 unless
/// `X-Pier-Federation: <plaintext>` resolves to an active row in
/// `federation_tokens`. On success it inserts a [`FederationContext`]
/// into `req.extensions()` and best-effort touches `last_used_at`.
///
/// This middleware is **only** wired in front of `/api/v1/agent/*` —
/// nothing else honours the X-Pier-Federation header. That isolation
/// is the whole point: a leaked federation token can deploy/restart
/// stacks but cannot touch user records, sessions, or admin endpoints.
pub async fn require_federation(
    State(state): State<SharedState>,
    mut req: Request,
    next: Next,
) -> Result<Response, AppError> {
    let plaintext = req
        .headers()
        .get(FEDERATION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or(AppError::Unauthorized)?;

    let token_hash = hash(&plaintext);

    let ctx = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT id, label FROM federation_tokens \
             WHERE token_hash = ?1 AND is_active = 1",
            [&token_hash],
            |row| {
                Ok(FederationContext {
                    token_id: row.get::<_, String>(0)?,
                    primary_label: row.get::<_, String>(1)?,
                })
            },
        )
        .ok()
        .ok_or(AppError::Unauthorized)?
    };

    // Best-effort touch — losing this update is harmless, so we don't
    // propagate errors. The DB lock is taken in a separate scope to
    // avoid holding it across the downstream `next.run`.
    {
        let now = chrono::Utc::now().timestamp();
        if let Ok(db) = state.db.lock() {
            let _ = db.execute(
                "UPDATE federation_tokens SET last_used_at = ?1 WHERE id = ?2",
                rusqlite::params![now, ctx.token_id],
            );
        }
    }

    req.extensions_mut().insert(ctx);
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_token_carries_prefix() {
        let t = generate();
        assert!(t.plaintext.starts_with(FEDERATION_PREFIX));
        assert_eq!(t.prefix.len(), DISPLAY_PREFIX_LEN);
        assert!(t.prefix.starts_with(FEDERATION_PREFIX));
    }

    #[test]
    fn two_tokens_differ() {
        let a = generate();
        let b = generate();
        assert_ne!(a.plaintext, b.plaintext);
    }

    #[test]
    fn hash_is_deterministic_and_isolated() {
        let t = "pier_fed_deadbeef";
        assert_eq!(hash(t), hash(t));
        // The hash function uses the same algorithm as server_token::hash
        // and api_token::hash, but the prefix space is disjoint so a
        // federation token can never collide with an agent token.
        assert_ne!(hash(t), hash("pier_srv_deadbeef"));
    }
}
