//! In-RAM store for short-lived "you cleared the password step, please
//! present the second factor" tokens issued by `POST /api/v1/auth/login`
//! when the user has TOTP enabled.
//!
//! Why not a JWT? Two reasons:
//!   1. We need true one-shot semantics. A JWT lives as long as its `exp`
//!      claim; revoking it after first use means maintaining a blacklist
//!      anyway. The HashMap already gives us that for free.
//!   2. Survival across process restarts is undesirable: if pier-core
//!      restarts between the password and TOTP step, the operator should
//!      retype the password. Volatile storage is the right default.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 5-minute window — long enough for the operator to fumble with the phone,
/// short enough that a stolen partial token isn't useful for long.
const PARTIAL_TTL_SECS: u64 = 300;

#[derive(Clone)]
struct Entry {
    user_id: String,
    expires_at: Instant,
    issued_to_ip: Option<IpAddr>,
}

pub struct PartialTokenStore {
    inner: Mutex<HashMap<String, Entry>>,
}

impl PartialTokenStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a fresh 32-byte hex token bound to `user_id` and (optionally) the
    /// caller's IP. Returns the token to send back in the JSON response body.
    pub fn issue(&self, user_id: String, issued_to_ip: Option<IpAddr>) -> String {
        use rand::RngExt;
        let bytes: [u8; 32] = rand::rng().random();
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let entry = Entry {
            user_id,
            expires_at: Instant::now() + Duration::from_secs(PARTIAL_TTL_SECS),
            issued_to_ip,
        };
        if let Ok(mut g) = self.inner.lock() {
            self.evict_expired(&mut g);
            g.insert(token.clone(), entry);
        }
        token
    }

    /// One-shot consume. Returns the bound `user_id` only when:
    ///   - the token exists and hasn't expired, AND
    ///   - if it was IP-bound at issue time, `ip` matches.
    ///
    /// In all cases the entry is removed.
    pub fn consume(&self, token: &str, ip: Option<IpAddr>) -> Option<String> {
        let mut g = self.inner.lock().ok()?;
        self.evict_expired(&mut g);
        let entry = g.remove(token)?;
        if let Some(bound) = entry.issued_to_ip {
            match ip {
                Some(now) if now == bound => Some(entry.user_id),
                _ => None,
            }
        } else {
            Some(entry.user_id)
        }
    }

    fn evict_expired(&self, g: &mut HashMap<String, Entry>) {
        let now = Instant::now();
        g.retain(|_, e| e.expires_at > now);
    }
}

impl Default for PartialTokenStore {
    fn default() -> Self {
        Self::new()
    }
}
