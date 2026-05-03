//! Bearer API tokens for the Pier HTTP API.
//!
//! Used by the embedded npm registry (`Authorization: Bearer pier_npm_…`) and
//! any other client (CI, CLI) that can't carry a session cookie.
//!
//! Tokens are stored in the `api_tokens` table as a sha256 hash; the plaintext
//! is shown to the user once at creation and never persisted. The same scheme
//! is used by GitHub PATs — leaking the DB does not leak the tokens.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::auth::middleware::AuthUser;

/// Prefix prepended to every issued token. Lets a leaked token be recognised
/// at a glance ("this is a Pier npm token") and gives us room to introduce
/// other token kinds later (`pier_cli_…`, `pier_ci_…`).
pub const TOKEN_PREFIX: &str = "pier_npm_";

/// Number of characters of the token (incl. prefix) shown in the UI as a
/// fingerprint. 16 is enough to be visually distinct, short enough to avoid
/// implying the rest can be guessed.
pub const DISPLAY_PREFIX_LEN: usize = 16;

/// A freshly issued token. `plaintext` is the only place the secret ever
/// exists — it is shown to the user once and dropped.
pub struct IssuedToken {
    pub id: String,
    pub plaintext: String,
    pub prefix: String,
}

/// Generate a new token. Caller is responsible for persisting via [`store`].
pub fn generate() -> IssuedToken {
    let bytes: [u8; 24] = rand::random();
    let plaintext = format!("{TOKEN_PREFIX}{}", hex::encode(bytes));
    let prefix = plaintext.chars().take(DISPLAY_PREFIX_LEN).collect();
    IssuedToken {
        id: uuid::Uuid::new_v4().to_string(),
        plaintext,
        prefix,
    }
}

/// sha256 hash of a plaintext token, hex-encoded. This is what lives in the DB.
pub fn hash(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

/// Persist a freshly issued token under the given user/name.
pub fn store(conn: &Connection, token: &IssuedToken, user_id: &str, name: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO api_tokens (id, user_id, name, token_hash, prefix, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            token.id,
            user_id,
            name,
            hash(&token.plaintext),
            token.prefix,
            now,
        ],
    )?;
    Ok(())
}

/// Look up the user behind a Bearer token. Returns `None` if the token is
/// unknown or revoked. Updates `last_used_at` on success (best-effort).
pub fn lookup(conn: &Connection, plaintext: &str) -> Result<Option<AuthUser>> {
    if !plaintext.starts_with(TOKEN_PREFIX) {
        return Ok(None);
    }
    let h = hash(plaintext);
    let row = conn
        .query_row(
            "SELECT t.id, u.id, u.username, u.role
             FROM api_tokens t
             JOIN users u ON u.id = t.user_id
             WHERE t.token_hash = ?1
               AND t.revoked_at IS NULL
               AND u.is_active = 1",
            [&h],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    let Some((token_id, user_id, username, role)) = row else {
        return Ok(None);
    };

    // Best-effort touch — never fail the request because we couldn't update a stat.
    let now = chrono::Utc::now().timestamp();
    let _ = conn.execute(
        "UPDATE api_tokens SET last_used_at = ?1 WHERE id = ?2",
        params![now, token_id],
    );

    Ok(Some(AuthUser {
        id: user_id,
        username,
        role,
        session_id: String::new(),
    }))
}

/// List all non-revoked tokens for a user (for the UI). Plaintext is never
/// returned — only the prefix is safe to display.
pub struct TokenSummary {
    pub id: String,
    pub name: String,
    pub prefix: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

pub fn list_for_user(conn: &Connection, user_id: &str) -> Result<Vec<TokenSummary>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, prefix, created_at, last_used_at
         FROM api_tokens
         WHERE user_id = ?1 AND revoked_at IS NULL
         ORDER BY created_at DESC",
    )?;
    let rows = stmt
        .query_map([user_id], |row| {
            Ok(TokenSummary {
                id: row.get(0)?,
                name: row.get(1)?,
                prefix: row.get(2)?,
                created_at: row.get(3)?,
                last_used_at: row.get::<_, Option<i64>>(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Mark a token as revoked. Idempotent — calling on an already-revoked token
/// is a no-op. Returns an error only if the token is owned by a different user.
pub fn revoke(conn: &Connection, token_id: &str, user_id: &str) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let updated = conn.execute(
        "UPDATE api_tokens
         SET revoked_at = ?1
         WHERE id = ?2 AND user_id = ?3 AND revoked_at IS NULL",
        params![now, token_id, user_id],
    )?;
    if updated == 0 {
        // Could be already revoked, wrong owner, or non-existent. Don't leak which.
        let exists: bool = conn
            .query_row(
                "SELECT 1 FROM api_tokens WHERE id = ?1 AND user_id = ?2",
                params![token_id, user_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if !exists {
            return Err(anyhow!("token not found"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_prefixed_unique_tokens() {
        let a = generate();
        let b = generate();
        assert!(a.plaintext.starts_with(TOKEN_PREFIX));
        assert!(b.plaintext.starts_with(TOKEN_PREFIX));
        assert_ne!(a.plaintext, b.plaintext);
        assert_eq!(a.prefix.len(), DISPLAY_PREFIX_LEN);
    }

    #[test]
    fn hash_is_deterministic() {
        let t = "pier_npm_deadbeef";
        assert_eq!(hash(t), hash(t));
        assert_ne!(hash(t), hash("pier_npm_other"));
    }

    #[test]
    fn lookup_rejects_non_pier_prefix() {
        // sqlite-less: function should short-circuit before touching the DB.
        let conn = Connection::open_in_memory().unwrap();
        let res = lookup(&conn, "ghp_somethingelse").unwrap();
        assert!(res.is_none());
    }
}
