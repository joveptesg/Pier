//! Boot-time reconciliation of services left mid-deploy.
//!
//! A deploy marks its `services` row `deploying` up front and the background
//! task settles it when the work ends. If the process dies in between — a
//! crash, an upgrade, an OOM kill during a large image pull — that task dies
//! with it and nothing is left to move the row. The service is then pinned at
//! `deploying` forever: `redeploy` and `restart` are the only actions offered
//! and both refuse, so the operator's only way out is to delete and recreate.
//!
//! On every boot we therefore settle those rows against reality. A running
//! container carrying the service's `pier.service.id` label means the deploy
//! actually landed before the interruption; anything else is reported as
//! failed, which is honest — we cannot know how far it got — and actionable,
//! because Redeploy works from there.

use anyhow::Result;
use bollard::query_parameters::ListContainersOptions;

use crate::state::AppState;

/// Settle every service still marked `deploying` by a previous process.
///
/// Best-effort and fire-and-forget: a Docker or DB failure is logged and
/// swallowed rather than blocking startup. A stale row is bad; refusing to
/// boot over one is worse.
pub fn run_on_boot(state: crate::state::SharedState) {
    tokio::spawn(async move {
        match reconcile(&state).await {
            Ok(0) => {}
            Ok(n) => tracing::info!("Settled {n} service(s) left mid-deploy by a previous process"),
            Err(e) => tracing::warn!("Deploy reconcile failed: {e}"),
        }
    });
}

async fn reconcile(state: &AppState) -> Result<usize> {
    // Lock is taken and dropped before any await — the guard is !Send.
    let stuck: Vec<(String, String)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare("SELECT id, name FROM services WHERE status = 'deploying'")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };
    if stuck.is_empty() {
        return Ok(0);
    }

    let containers = state
        .docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?;

    let mut settled = 0usize;
    for (service_id, name) in stuck {
        let is_running = containers.iter().any(|c| {
            let labelled = c
                .labels
                .as_ref()
                .and_then(|l| l.get("pier.service.id"))
                .is_some_and(|s| *s == service_id);
            let running = c
                .state
                .as_ref()
                .map(|s| format!("{s:?}").to_lowercase())
                .is_some_and(|s| s == "running");
            labelled && running
        });

        let (service_status, log_status, note) = if is_running {
            (
                "running",
                "success",
                "Deploy had finished before the restart; status recovered on boot.",
            )
        } else {
            (
                "failed",
                "failed",
                "Deploy was interrupted by a restart and never completed. Redeploy to retry.",
            )
        };

        if let Ok(db) = state.db.lock() {
            // Guarded on the old value: a deploy started since the list was
            // read must not be clobbered.
            let _ = db.execute(
                "UPDATE services SET status = ?1, updated_at = datetime('now')
                 WHERE id = ?2 AND status = 'deploying'",
                rusqlite::params![service_status, service_id],
            );
            // Close the log row the dead task opened, so the panel does not
            // show a deploy that is still 'running' hours later.
            let _ = db.execute(
                "UPDATE deployment_logs
                    SET status = ?1, finished_at = datetime('now'),
                        output = output || ?2
                  WHERE service_id = ?3 AND finished_at IS NULL",
                rusqlite::params![log_status, format!("\n{note}\n"), service_id],
            );
        }

        tracing::info!("Service '{name}' was stuck at 'deploying' — settled as {service_status}");
        settled += 1;
    }

    Ok(settled)
}
