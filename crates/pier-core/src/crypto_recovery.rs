use base64::{engine::general_purpose::STANDARD as B64, Engine};

use crate::crypto;
use crate::state::SharedState;

/// Filename dropped into the data dir by `install.sh` containing historical
/// `PIER_SECRET` values harvested from journald. One base64-encoded key per line.
const RECOVERY_FILE: &str = ".pier-recovery-keys";

/// If `{data_dir}/.pier-recovery-keys` exists, try each listed key against
/// every `ENC:...` row in `services.env_json`. Successfully decrypted rows are
/// re-encrypted with the current stable key and written back. The recovery
/// file is removed on completion so the sweep runs at most once per file drop.
pub fn run_recovery_if_needed(state: &SharedState) {
    let path = state.config.data_dir.join(RECOVERY_FILE);
    if !path.exists() {
        return;
    }

    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("Recovery file unreadable: {e}");
            return;
        }
    };

    let keys: Vec<[u8; 32]> = raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let bytes = B64.decode(trimmed).ok()?;
            if bytes.len() < 32 {
                return None;
            }
            let mut k = [0u8; 32];
            k.copy_from_slice(&bytes[..32]);
            Some(k)
        })
        .collect();

    if keys.is_empty() {
        tracing::info!("Recovery file had no valid keys — removing");
        let _ = std::fs::remove_file(&path);
        return;
    }

    tracing::info!("Starting env_json recovery with {} candidate keys", keys.len());

    let stable_key = crypto::get_secret_key();

    let rows: Vec<(String, String)> = {
        let db = match state.db.lock() {
            Ok(db) => db,
            Err(e) => {
                tracing::error!("Recovery DB lock failed: {e}");
                return;
            }
        };
        let mut stmt = match db.prepare(
            "SELECT id, env_json FROM services WHERE env_json LIKE 'ENC:%'",
        ) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Recovery prepare failed: {e}");
                return;
            }
        };
        let iter = match stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            Ok(it) => it,
            Err(e) => {
                tracing::error!("Recovery query failed: {e}");
                return;
            }
        };
        iter.filter_map(|r| r.ok()).collect()
    };

    if rows.is_empty() {
        tracing::info!("No ENC: rows to recover — removing recovery file");
        let _ = std::fs::remove_file(&path);
        return;
    }

    let mut recovered = 0usize;
    let mut already_ok = 0usize;
    let mut lost = 0usize;

    for (id, enc) in &rows {
        // Try the stable key first — if we get a hit, the row is already
        // fine and we don't need to touch it.
        if let Ok(plain) = crypto::decrypt(enc, &stable_key) {
            if serde_json::from_str::<serde_json::Value>(&plain).is_ok() {
                already_ok += 1;
                continue;
            }
        }

        let restored: Option<String> = keys.iter().find_map(|k| {
            let plain = crypto::decrypt(enc, k).ok()?;
            if serde_json::from_str::<serde_json::Value>(&plain).is_ok() {
                Some(plain)
            } else {
                None
            }
        });

        match restored {
            Some(plain) => {
                match crypto::encrypt(&plain, &stable_key) {
                    Ok(re_enc) => {
                        if let Ok(db) = state.db.lock() {
                            let _ = db.execute(
                                "UPDATE services SET env_json = ?1, updated_at = datetime('now') WHERE id = ?2",
                                rusqlite::params![re_enc, id],
                            );
                        }
                        recovered += 1;
                    }
                    Err(e) => {
                        tracing::error!("Re-encrypt failed for {id}: {e}");
                        lost += 1;
                    }
                }
            }
            None => {
                lost += 1;
                tracing::warn!(
                    "Service {id}: env_json cannot be recovered with any known key"
                );
            }
        }
    }

    tracing::info!(
        "env_json recovery done: {recovered} restored, {already_ok} already readable, {lost} unrecoverable (of {} ENC rows)",
        rows.len()
    );
    if let Err(e) = std::fs::remove_file(&path) {
        tracing::warn!("Could not remove recovery file: {e}");
    }
}
