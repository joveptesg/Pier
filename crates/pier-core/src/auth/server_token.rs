//! Tokens that authenticate **remote servers** (agents and peers), not users.
//!
//! Two distinct token kinds live in the `servers` table:
//!
//! * `pier_boot_…` — short-lived (1h) bootstrap token printed in the install
//!   command. Spent exactly once when the agent calls `/handshake`.
//! * `pier_srv_…`  — long-lived agent/peer credential. Used in heartbeats and
//!   as the outbound `Authorization: Bearer …` core→agent.
//!
//! Both are stored as sha256 hashes (`bootstrap_token_hash` /
//! `agent_token_hash`) — the plaintext is shown to the operator (or returned
//! to the agent) once and never persisted. This mirrors the GitHub-PAT
//! pattern already used for user API tokens in [`super::api_token`].

use sha2::{Digest, Sha256};

/// Prefix for the short-lived bootstrap credential issued at server creation.
/// Lets a leaked token be recognised at a glance ("this is a Pier bootstrap")
/// and lets future code refuse a bootstrap where a long-term token is expected
/// (and vice versa).
pub const BOOTSTRAP_PREFIX: &str = "pier_boot_";

/// Prefix for the long-lived agent / peer credential. `pier_srv_…` shows up in
/// `/etc/pier-agent/auth.env` and in outgoing `Authorization: Bearer` headers.
pub const AGENT_PREFIX: &str = "pier_srv_";

/// Number of characters of the token (incl. prefix) shown in the UI as a
/// fingerprint. 16 is enough to be visually distinct, short enough to avoid
/// implying the rest can be guessed.
pub const DISPLAY_PREFIX_LEN: usize = 16;

/// Bootstrap lifetime: an operator who copies the install command should have
/// reasonable time to SSH onto the box and run it, but not days. Long enough
/// for "I'll do it after lunch", short enough that a leaked install URL
/// expires by the next morning.
pub const BOOTSTRAP_TTL_SECS: i64 = 60 * 60;

/// A freshly minted token. `plaintext` exists only at issue time — store the
/// hash, return the plaintext to the caller, then drop.
pub struct IssuedToken {
    pub plaintext: String,
    pub prefix: String,
}

/// Generate a token with the requested prefix. 24 random bytes hex-encoded
/// gives 48 chars of entropy after the prefix — safely above brute-force
/// reach even without rate limiting.
fn generate_with_prefix(prefix: &str) -> IssuedToken {
    let bytes: [u8; 24] = rand::random();
    let plaintext = format!("{prefix}{}", hex::encode(bytes));
    let display_prefix = plaintext.chars().take(DISPLAY_PREFIX_LEN).collect();
    IssuedToken {
        plaintext,
        prefix: display_prefix,
    }
}

/// Issue a bootstrap token. Stored as hash + `bootstrap_expires_at` so the
/// row alone can't authenticate after the TTL.
pub fn generate_bootstrap() -> IssuedToken {
    generate_with_prefix(BOOTSTRAP_PREFIX)
}

/// Issue a long-lived agent/peer token. Returned to the agent exactly once
/// over the handshake response; the agent persists the plaintext in its
/// systemd `Environment=` file, core keeps only the hash.
pub fn generate_agent() -> IssuedToken {
    generate_with_prefix(AGENT_PREFIX)
}

/// sha256-hex of a plaintext token. Identical encoding to
/// [`super::api_token::hash`] but kept as its own function so callers can't
/// accidentally cross-validate a user API token against a server token.
pub fn hash(plaintext: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(plaintext.as_bytes());
    hex::encode(hasher.finalize())
}

/// True when the bootstrap is still within its TTL. `expires_at` is a Unix
/// timestamp; `None` means the row was never bootstrap-tokened (legacy or
/// already-redeemed) and must not authenticate as a bootstrap.
pub fn bootstrap_alive(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|exp| exp > now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_bootstrap_carries_prefix() {
        let t = generate_bootstrap();
        assert!(t.plaintext.starts_with(BOOTSTRAP_PREFIX));
        assert_eq!(t.prefix.len(), DISPLAY_PREFIX_LEN);
        assert!(t.prefix.starts_with(BOOTSTRAP_PREFIX));
    }

    #[test]
    fn generate_agent_carries_prefix() {
        let t = generate_agent();
        assert!(t.plaintext.starts_with(AGENT_PREFIX));
        assert!(t.prefix.starts_with(AGENT_PREFIX));
    }

    #[test]
    fn two_tokens_differ() {
        let a = generate_agent();
        let b = generate_agent();
        assert_ne!(a.plaintext, b.plaintext);
    }

    #[test]
    fn hash_is_deterministic_and_distinct() {
        let t = "pier_srv_deadbeef";
        assert_eq!(hash(t), hash(t));
        assert_ne!(hash(t), hash("pier_srv_other"));
    }

    #[test]
    fn bootstrap_alive_respects_ttl() {
        assert!(bootstrap_alive(Some(100), 50));
        assert!(!bootstrap_alive(Some(50), 100));
        assert!(!bootstrap_alive(Some(100), 100), "expiry is strict >");
        assert!(!bootstrap_alive(None, 50));
    }
}
