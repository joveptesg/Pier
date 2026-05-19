//! HTTP API for user-defined schedules. Admin-gated by the parent router.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::scheduler::{actions, cron_utils};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateScheduleRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub cron_expression: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
    pub action_type: String,
    pub action_config: serde_json::Value,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_tz() -> String {
    "UTC".to_string()
}
fn default_enabled() -> bool {
    true
}

#[derive(Deserialize)]
pub struct UpdateScheduleRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub cron_expression: Option<String>,
    pub timezone: Option<String>,
    pub action_config: Option<serde_json::Value>,
    pub enabled: Option<bool>,
}

#[derive(Deserialize)]
pub struct ValidateCronRequest {
    pub cron_expression: String,
    #[serde(default = "default_tz")]
    pub timezone: String,
}

const ALLOWED_TYPES: &[&str] = &["task", "backup", "cleanup"];

fn check_action_type(t: &str) -> AppResult<()> {
    if !ALLOWED_TYPES.contains(&t) {
        return Err(AppError::BadRequest(format!("unknown action_type '{t}'")));
    }
    Ok(())
}

pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, description, cron_expression, timezone, action_type, action_config,
                enabled, misfire_policy, last_run_at, next_run_at, last_status, last_error,
                is_system, created_at
         FROM schedules ORDER BY name ASC",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            Ok(serde_json::json!({
                "id":              r.get::<_, String>(0)?,
                "name":            r.get::<_, String>(1)?,
                "description":     r.get::<_, String>(2)?,
                "cron_expression": r.get::<_, String>(3)?,
                "timezone":        r.get::<_, String>(4)?,
                "action_type":     r.get::<_, String>(5)?,
                "action_config":   r.get::<_, String>(6)?,
                "enabled":         r.get::<_, bool>(7)?,
                "misfire_policy":  r.get::<_, String>(8)?,
                "last_run_at":     r.get::<_, Option<String>>(9)?,
                "next_run_at":     r.get::<_, Option<String>>(10)?,
                "last_status":     r.get::<_, Option<String>>(11)?,
                "last_error":      r.get::<_, Option<String>>(12)?,
                "is_system":       r.get::<_, bool>(13)?,
                "created_at":      r.get::<_, String>(14)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(rows))
}

pub async fn create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<CreateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest("name required".into()));
    }
    check_action_type(&body.action_type)?;
    cron_utils::parse(&body.cron_expression).map_err(|e| AppError::BadRequest(e.to_string()))?;

    let id = uuid::Uuid::new_v4().to_string();
    let config_str = serde_json::to_string(&body.action_config).unwrap_or_else(|_| "{}".into());
    let next = cron_utils::next_fire_utc(&body.cron_expression, &body.timezone, chrono::Utc::now())
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO schedules
            (id, name, description, cron_expression, timezone, action_type, action_config,
             enabled, created_by, next_run_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            id,
            body.name.trim(),
            body.description.unwrap_or_default(),
            body.cron_expression,
            body.timezone,
            body.action_type,
            config_str,
            body.enabled as i64,
            user.id,
            next,
        ],
    )?;
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let row: Option<serde_json::Value> = db
        .query_row(
            "SELECT id, name, description, cron_expression, timezone, action_type, action_config,
                    enabled, misfire_policy, last_run_at, next_run_at, last_status, last_error,
                    is_system, created_at
             FROM schedules WHERE id = ?1",
            [&id],
            |r| {
                Ok(serde_json::json!({
                    "id":              r.get::<_, String>(0)?,
                    "name":            r.get::<_, String>(1)?,
                    "description":     r.get::<_, String>(2)?,
                    "cron_expression": r.get::<_, String>(3)?,
                    "timezone":        r.get::<_, String>(4)?,
                    "action_type":     r.get::<_, String>(5)?,
                    "action_config":   r.get::<_, String>(6)?,
                    "enabled":         r.get::<_, bool>(7)?,
                    "misfire_policy":  r.get::<_, String>(8)?,
                    "last_run_at":     r.get::<_, Option<String>>(9)?,
                    "next_run_at":     r.get::<_, Option<String>>(10)?,
                    "last_status":     r.get::<_, Option<String>>(11)?,
                    "last_error":      r.get::<_, Option<String>>(12)?,
                    "is_system":       r.get::<_, bool>(13)?,
                    "created_at":      r.get::<_, String>(14)?,
                }))
            },
        )
        .optional()?;
    let row = row.ok_or_else(|| AppError::NotFound("schedule not found".into()))?;

    // Last 25 runs for the history panel.
    let mut stmt = db.prepare(
        "SELECT id, started_at, finished_at, status, triggered_by, output, error, task_run_id
         FROM schedule_runs WHERE schedule_id = ?1
         ORDER BY started_at DESC LIMIT 25",
    )?;
    let runs: Vec<serde_json::Value> = stmt
        .query_map([&id], |r| {
            Ok(serde_json::json!({
                "id":           r.get::<_, String>(0)?,
                "started_at":   r.get::<_, String>(1)?,
                "finished_at":  r.get::<_, Option<String>>(2)?,
                "status":       r.get::<_, String>(3)?,
                "triggered_by": r.get::<_, String>(4)?,
                "output":       r.get::<_, String>(5)?,
                "error":        r.get::<_, Option<String>>(6)?,
                "task_run_id":  r.get::<_, Option<String>>(7)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(serde_json::json!({
        "schedule": row,
        "runs": runs,
    })))
}

pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    if let Some(c) = &body.cron_expression {
        cron_utils::parse(c).map_err(|e| AppError::BadRequest(e.to_string()))?;
    }
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    if let Some(name) = body.name {
        db.execute(
            "UPDATE schedules SET name=?1, updated_at=datetime('now') WHERE id=?2",
            params![name, id],
        )?;
    }
    if let Some(d) = body.description {
        db.execute(
            "UPDATE schedules SET description=?1, updated_at=datetime('now') WHERE id=?2",
            params![d, id],
        )?;
    }
    if let Some(c) = body.cron_expression {
        // Recompute next_run_at.
        let tz: String =
            db.query_row("SELECT timezone FROM schedules WHERE id = ?1", [&id], |r| {
                r.get(0)
            })?;
        let next = cron_utils::next_fire_utc(&c, &tz, chrono::Utc::now())
            .ok()
            .flatten()
            .map(|t| t.to_rfc3339());
        db.execute(
            "UPDATE schedules SET cron_expression=?1, next_run_at=?2, updated_at=datetime('now') WHERE id=?3",
            params![c, next, id],
        )?;
    }
    if let Some(tz) = body.timezone {
        let cron: String = db.query_row(
            "SELECT cron_expression FROM schedules WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )?;
        let next = cron_utils::next_fire_utc(&cron, &tz, chrono::Utc::now())
            .ok()
            .flatten()
            .map(|t| t.to_rfc3339());
        db.execute(
            "UPDATE schedules SET timezone=?1, next_run_at=?2, updated_at=datetime('now') WHERE id=?3",
            params![tz, next, id],
        )?;
    }
    if let Some(cfg) = body.action_config {
        let cfg_str = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".into());
        db.execute(
            "UPDATE schedules SET action_config=?1, updated_at=datetime('now') WHERE id=?2",
            params![cfg_str, id],
        )?;
    }
    if let Some(en) = body.enabled {
        db.execute(
            "UPDATE schedules SET enabled=?1, updated_at=datetime('now') WHERE id=?2",
            params![en as i64, id],
        )?;
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let is_system: Option<bool> = db
        .query_row(
            "SELECT is_system FROM schedules WHERE id = ?1",
            [&id],
            |r| r.get(0),
        )
        .optional()?;
    match is_system {
        None => return Err(AppError::NotFound("schedule not found".into())),
        Some(true) => {
            return Err(AppError::Conflict(
                "system schedules can be disabled but not deleted".into(),
            ))
        }
        Some(false) => {}
    }
    db.execute("DELETE FROM schedules WHERE id = ?1", [&id])?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn run_now(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (action_type, action_config) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT action_type, action_config FROM schedules WHERE id = ?1",
            [&id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::NotFound("schedule not found".into()))?
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO schedule_runs (id, schedule_id, triggered_by, status)
             VALUES (?1, ?2, 'manual', 'running')",
            params![run_id, id],
        )?;
    }

    let result = actions::dispatch(&state, &id, &action_type, &action_config, "manual").await;
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
                SET finished_at = datetime('now'), status = ?1, output = ?2, error = ?3,
                    task_run_id = ?4
              WHERE id = ?5",
            params![status, output, error, task_run_id, run_id],
        );
    }
    Ok(Json(serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "status": status,
        "task_run_id": task_run_id,
    })))
}

pub async fn enable(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "UPDATE schedules SET enabled = 1, updated_at = datetime('now') WHERE id = ?1",
        [&id],
    )?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn disable(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "UPDATE schedules SET enabled = 0, updated_at = datetime('now') WHERE id = ?1",
        [&id],
    )?;
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn validate_cron(Json(body): Json<ValidateCronRequest>) -> AppResult<impl IntoResponse> {
    let preview = cron_utils::preview(&body.cron_expression, &body.timezone, chrono::Utc::now(), 5)
        .map_err(|e| AppError::BadRequest(e.to_string()))?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "next_runs": preview,
    })))
}
