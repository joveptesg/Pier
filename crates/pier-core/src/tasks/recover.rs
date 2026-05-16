//! On-boot recovery for in-flight task runs.
//!
//! Run on core startup. For every `task_runs` row in a non-terminal state:
//!
//! * If the agent still knows the `agent_run_id`, attach a fresh poller —
//!   the run is alive, we just lost the in-process driver to the restart.
//! * If the agent answers 404 (it restarted too, or the GC kicked in), we
//!   mark the row `unreachable` so the UI moves on.
//! * If we can't reach the agent at all, leave the row as-is and the
//!   user can retry — the agent might come back.

use crate::state::SharedState;
use crate::tasks::{executor, models};

pub fn run_on_boot(state: SharedState) {
    tokio::spawn(async move {
        let active = {
            let db = match state.db.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            match models::list_active_runs(&db) {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!("task recovery: list_active_runs failed: {e}");
                    return;
                }
            }
        };
        if active.is_empty() {
            return;
        }
        tracing::info!("task recovery: resuming {} active run(s)", active.len());
        for run in active {
            if let Some(agent_run_id) = run.agent_run_id {
                executor::spawn_driver(state.clone(), run.id, run.server_id, agent_run_id);
            } else {
                // Pending row that never got past the `/shell` POST — we
                // can't recover its identity on the agent side, so treat
                // it as never-started.
                let db = match state.db.lock() {
                    Ok(g) => g,
                    Err(_) => continue,
                };
                let _ = models::run_mark_unreachable(
                    &db,
                    &run.id,
                    "core restarted before agent acknowledged",
                );
            }
        }
    });
}
