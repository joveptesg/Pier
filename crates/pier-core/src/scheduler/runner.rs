//! The 60-second cron tick that fires due schedules.
//!
//! Concurrency safety: an in-process `HashSet<schedule_id>` tracks
//! currently-running fires. A second tick that finds the same schedule
//! still in flight records a `skipped` run instead of double-firing.
//!
//! Misfire handling: on every tick a schedule's `next_run_at` is
//! recomputed via `cron_utils::next_fire_utc`. If a row has
//! `next_run_at IS NULL` (just created / cron updated) it's recomputed
//! the same way. We never replay missed runs — a long downtime simply
//! skips them.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use rusqlite::params;
use tokio::sync::Mutex;

use crate::scheduler::{actions, cron_utils};
use crate::state::SharedState;

/// Cadence of the master tick. 60 s is fine-grained enough for cron-level
/// precision while leaving the SQLite mutex free between scans.
const TICK: Duration = Duration::from_secs(60);

/// Spawn the runner. Idempotent at the type level (call once at boot).
pub fn start(state: SharedState) {
    let in_flight: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
    tokio::spawn(async move {
        // Tiny startup grace so DB migrations + other boot work settle
        // before we start firing things.
        tokio::time::sleep(Duration::from_secs(5)).await;

        // First pass: backfill next_run_at for any row that's missing it,
        // so the immediate tick has accurate due times.
        seed_next_run_at(&state);

        let mut ticker = tokio::time::interval(TICK);
        // Skip the immediate first tick — we just seeded.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            run_tick(&state, &in_flight).await;
        }
    });
}

fn seed_next_run_at(state: &SharedState) {
    let db = match state.db.lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    let mut stmt = match db.prepare(
        "SELECT id, cron_expression, timezone FROM schedules
         WHERE enabled = 1 AND next_run_at IS NULL",
    ) {
        Ok(s) => s,
        Err(_) => return,
    };
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .map(|it| it.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();
    drop(stmt);

    let now = Utc::now();
    for (id, cron, tz) in rows {
        if let Ok(Some(next)) = cron_utils::next_fire_utc(&cron, &tz, now) {
            let _ = db.execute(
                "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
                params![next.to_rfc3339(), id],
            );
        }
    }
}

async fn run_tick(state: &SharedState, in_flight: &Arc<Mutex<HashSet<String>>>) {
    let due = match find_due(state) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::warn!("scheduler: due query failed: {e}");
            return;
        }
    };
    for row in due {
        // Double-fire guard.
        let already = {
            let mut guard = in_flight.lock().await;
            if guard.contains(&row.id) {
                true
            } else {
                guard.insert(row.id.clone());
                false
            }
        };
        if already {
            record_skipped(state, &row.id, "previous run still in flight");
            advance_next_run_at(state, &row);
            continue;
        }

        let state_cl = state.clone();
        let in_flight_cl = in_flight.clone();
        let row_cl = row.clone();
        tokio::spawn(async move {
            run_one(&state_cl, &row_cl).await;
            in_flight_cl.lock().await.remove(&row_cl.id);
        });
    }
}

#[derive(Clone)]
struct DueRow {
    id: String,
    cron_expression: String,
    timezone: String,
    action_type: String,
    action_config: String,
}

fn find_due(state: &SharedState) -> anyhow::Result<Vec<DueRow>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, cron_expression, timezone, action_type, action_config
         FROM schedules
         WHERE enabled = 1
           AND next_run_at IS NOT NULL
           AND datetime(next_run_at) <= datetime('now')",
    )?;
    let rows: Vec<DueRow> = stmt
        .query_map([], |r| {
            Ok(DueRow {
                id: r.get(0)?,
                cron_expression: r.get(1)?,
                timezone: r.get(2)?,
                action_type: r.get(3)?,
                action_config: r.get(4)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

async fn run_one(state: &SharedState, row: &DueRow) {
    let run_id = uuid::Uuid::new_v4().to_string();
    let started = chrono::Utc::now().to_rfc3339();

    // Insert running schedule_run.
    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "INSERT INTO schedule_runs (id, schedule_id, started_at, triggered_by, status)
             VALUES (?1, ?2, ?3, 'cron', 'running')",
            params![run_id, row.id, started],
        );
    }

    let result =
        actions::dispatch(state, &row.id, &row.action_type, &row.action_config, "cron").await;

    let (status, output, error, task_run_id) = match result {
        Ok(ar) => ("success".to_string(), ar.output, None, ar.task_run_id),
        Err(e) => (
            "failed".to_string(),
            String::new(),
            Some(format!("{e:#}")),
            None,
        ),
    };

    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "UPDATE schedule_runs
                SET finished_at = ?1, status = ?2, output = ?3, error = ?4,
                    task_run_id = ?5
              WHERE id = ?6",
            params![
                chrono::Utc::now().to_rfc3339(),
                status,
                output,
                error,
                task_run_id,
                run_id,
            ],
        );
        let _ = db.execute(
            "UPDATE schedules
                SET last_run_at = datetime('now'),
                    last_status = ?1,
                    last_error  = ?2
              WHERE id = ?3",
            params![status, error, row.id],
        );
    }

    advance_next_run_at(state, row);
}

fn advance_next_run_at(state: &SharedState, row: &DueRow) {
    let now = Utc::now();
    let next = match cron_utils::next_fire_utc(&row.cron_expression, &row.timezone, now) {
        Ok(Some(n)) => n,
        _ => return,
    };
    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "UPDATE schedules SET next_run_at = ?1 WHERE id = ?2",
            params![next.to_rfc3339(), row.id],
        );
    }
}

fn record_skipped(state: &SharedState, schedule_id: &str, reason: &str) {
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "INSERT INTO schedule_runs
                (id, schedule_id, started_at, finished_at, status, triggered_by, output)
             VALUES (?1, ?2, ?3, ?3, 'skipped', 'cron', ?4)",
            params![id, schedule_id, now, reason],
        );
    }
}
