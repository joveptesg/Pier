//! One-shot token guarding `/setup` until the first admin account exists.
//!
//! `install.sh` writes a 32-byte base64url token to `${PIER_DATA_DIR}/.setup_token`
//! and prints the corresponding `/setup?token=…` URL to the operator. Pier-core
//! loads the file once at startup and refuses to serve `/setup` (returns 404) for
//! any request that doesn't carry the matching token. Atomic insert + per-IP
//! rate limit already cover the race for first admin; this closes the orthogonal
//! window where the setup page is publicly reachable on a fresh VPS.
//!
//! Fallback behaviour: if the file is missing at startup (operator removed it,
//! upgraded from a pre-token version, or run outside install.sh) we log a WARN
//! and serve `/setup` unauthenticated — bricking the panel on an honest mistake
//! would be worse than the current baseline.

use std::path::PathBuf;
use std::sync::Mutex;

/// Persisted-on-disk one-shot token store. Held in `AppState` as `Arc<Self>`.
pub struct SetupTokenStore {
    path: PathBuf,
    token: Mutex<Option<String>>,
}

impl SetupTokenStore {
    /// Read `path` into RAM. A missing or empty file leaves the store in
    /// "unset" mode (legacy behaviour — `/setup` open). Logged at INFO/WARN.
    ///
    /// `users_exist` lets the store proactively delete a stale token left
    /// behind by a legacy install where the admin was created before
    /// `consume()` was wired up. install.sh later treats the file's presence
    /// as the canonical "admin not yet created" signal, so a stray token
    /// would cause it to print the `/setup?token=…` URL on every upgrade
    /// even when setup is long complete.
    pub fn load(path: PathBuf, users_exist: bool) -> Self {
        if users_exist {
            match std::fs::remove_file(&path) {
                Ok(()) => tracing::info!(
                    "Removed stale setup token at {} (admin already exists)",
                    path.display()
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    "Stale setup token unlink failed ({e}); please delete {} manually",
                    path.display()
                ),
            }
            return Self {
                path,
                token: Mutex::new(None),
            };
        }

        let token = std::fs::read_to_string(&path)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        match &token {
            Some(_) => tracing::info!("Setup token loaded from {}", path.display()),
            None => tracing::warn!(
                "No setup token at {} — /setup will be served unauthenticated until the first admin is created",
                path.display()
            ),
        }
        Self {
            path,
            token: Mutex::new(token),
        }
    }

    /// True when a token is loaded (i.e. clients must supply `?token=…`).
    pub fn is_required(&self) -> bool {
        self.token.lock().ok().map(|g| g.is_some()).unwrap_or(false)
    }

    /// Constant-time comparison against the loaded token. Returns false if the
    /// store is unset (callers should branch on `is_required` first) or if the
    /// internal mutex is poisoned.
    pub fn matches(&self, provided: &str) -> bool {
        let Ok(guard) = self.token.lock() else {
            return false;
        };
        let Some(stored) = guard.as_ref() else {
            return false;
        };
        constant_time_eq(stored.as_bytes(), provided.as_bytes())
    }

    /// After successful first-admin creation: delete the file from disk and
    /// clear the in-RAM copy. Best-effort — a failed unlink does not roll back
    /// the user creation; the next `/setup` will still 302 to `/login`.
    pub fn consume(&self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    "Setup token unlink failed ({e}); please delete {} manually",
                    self.path.display()
                );
            }
        }
        if let Ok(mut g) = self.token.lock() {
            *g = None;
        }
    }
}

/// Length-checked, branch-free byte comparison. We avoid the `subtle` crate to
/// keep the dependency surface small — this is the only place we need it.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_for_exact_token() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".setup_token");
        std::fs::write(&p, "abc123\n").unwrap();
        let store = SetupTokenStore::load(p, false);
        assert!(store.is_required());
        assert!(store.matches("abc123"));
        assert!(!store.matches("abc124"));
        assert!(!store.matches(""));
    }

    #[test]
    fn missing_file_is_unset_mode() {
        let dir = tempfile::tempdir().unwrap();
        let store = SetupTokenStore::load(dir.path().join("absent"), false);
        assert!(!store.is_required());
        assert!(!store.matches("anything"));
    }

    #[test]
    fn consume_removes_disk_and_ram() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".setup_token");
        std::fs::write(&p, "tok").unwrap();
        let store = SetupTokenStore::load(p.clone(), false);
        assert!(store.is_required());
        store.consume();
        assert!(!p.exists());
        assert!(!store.is_required());
    }

    #[test]
    fn stale_token_is_deleted_when_users_exist() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".setup_token");
        std::fs::write(&p, "legacy-token").unwrap();
        let store = SetupTokenStore::load(p.clone(), true);
        assert!(!p.exists(), "stale token file should be removed on load");
        assert!(!store.is_required());
        assert!(!store.matches("legacy-token"));
    }

    #[test]
    fn users_exist_with_no_file_is_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("absent");
        let store = SetupTokenStore::load(p, true);
        assert!(!store.is_required());
    }
}
