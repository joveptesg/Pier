//! Traefik watchdog: bring the proxy back when it is down and nobody meant it.
//!
//! The container runs with `restart: unless-stopped`, which covers crashes
//! but not an explicit stop — and an explicit stop is exactly what a redeploy
//! interrupted halfway leaves behind (`Exited (0)`, every site answering 521).
//! `deploy_traefik` is cancellation-safe now, but this is the backstop for the
//! next bug of that shape and for a stray manual `docker stop`.
//!
//! Heals only when the proxy is enabled in settings (an operator's "Disable"
//! is respected), no deploy holds the lock, and the container was seen down
//! on two consecutive checks — a redeploy's own stop→start gap or an
//! update's hand-off to its rollback must not trigger a competing deploy.

use std::time::{Duration, Instant};

use crate::state::SharedState;

const CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// After a failed heal, wait before pulling/recreating again.
const FAILURE_BACKOFF: Duration = Duration::from_secs(300);
/// Consecutive "down" observations required before healing.
const DOWN_CHECKS_BEFORE_HEAL: u32 = 2;

pub fn start(state: SharedState) {
    tokio::spawn(async move {
        let mut down_streak = 0u32;
        let mut retry_at = Instant::now();
        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;

            if !proxy_enabled(&state) || super::deploy_in_progress() {
                down_streak = 0;
                continue;
            }
            let running = match super::traefik_status(&state.docker).await {
                Ok(s) => s.running,
                Err(_) => continue,
            };
            down_streak = if running { 0 } else { down_streak + 1 };
            if !should_heal(down_streak, Instant::now() >= retry_at) {
                continue;
            }

            tracing::warn!("Traefik is not running although the proxy is enabled — redeploying");
            match heal(&state).await {
                Ok(version) => {
                    tracing::info!("Traefik {version} restored by watchdog");
                    down_streak = 0;
                }
                Err(e) => {
                    tracing::error!(
                        "Watchdog could not restore Traefik: {e}. Next attempt in {}s",
                        FAILURE_BACKOFF.as_secs()
                    );
                    retry_at = Instant::now() + FAILURE_BACKOFF;
                }
            }
        }
    });
}

fn should_heal(down_streak: u32, backoff_elapsed: bool) -> bool {
    down_streak >= DOWN_CHECKS_BEFORE_HEAL && backoff_elapsed
}

/// Redeploy the configured version; if that fails, fall back to the version
/// that ran before the last update and make it the configured one.
async fn heal(state: &SharedState) -> anyhow::Result<String> {
    let (current, previous) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let get = |key: &str| {
            db.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get::<_, String>(0)
            })
            .ok()
            .filter(|v| !v.is_empty())
        };
        (
            get("proxy.traefik_version")
                .unwrap_or_else(|| super::DEFAULT_TRAEFIK_VERSION.to_string()),
            get("proxy.traefik_version_previous"),
        )
    };

    let err = match super::redeploy_from_settings(state, &current).await {
        Ok(()) => return Ok(current),
        Err(e) => e,
    };
    let Some(previous) = previous.filter(|p| *p != current) else {
        return Err(err);
    };
    tracing::warn!("Traefik {current} failed to start ({err}); trying previous {previous}");
    super::redeploy_from_settings(state, &previous).await?;
    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.traefik_version', ?1)",
            [&previous],
        );
    }
    Ok(previous)
}

fn proxy_enabled(state: &SharedState) -> bool {
    let Ok(db) = state.db.lock() else {
        return false;
    };
    db.query_row(
        "SELECT value FROM settings WHERE key = 'proxy.enabled'",
        [],
        |row| row.get::<_, String>(0),
    )
    .map(|v| v != "false")
    .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heals_only_after_consecutive_down_checks() {
        assert!(!should_heal(0, true));
        assert!(!should_heal(1, true));
        assert!(should_heal(2, true));
        assert!(should_heal(5, true));
    }

    #[test]
    fn respects_backoff() {
        assert!(!should_heal(3, false));
    }
}
