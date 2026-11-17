//! Scheduled env exports — System → Backups → Env export. Admin only (the
//! routes sit in the `admin_only` router): an export spans every project.
//! Execution lives in `crate::export::scheduled`.

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::Connection;
use serde::Deserialize;

use crate::api::backups::{download_blob, validate_cron};
use crate::api::export::MIN_ARCHIVE_PASSWORD;
use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::export::scheduled::{self, sync_unified_for_export};
use crate::state::SharedState;

/// Body of POST / PUT /api/v1/export-schedules. PUT replaces every field;
/// an empty `archive_password` on PUT keeps the stored one.
#[derive(Deserialize)]
pub struct ExportScheduleBody {
    pub name: String,
    pub s3_storage_id: String,
    pub cron_expression: String,
    #[serde(default = "default_retention")]
    pub retention_count: i64,
    /// `None` = every project, including ones created later.
    #[serde(default)]
    pub project_ids: Option<Vec<String>>,
    #[serde(default = "default_true")]
    pub include_unassigned: bool,
    #[serde(default)]
    pub include_dumps: bool,
    #[serde(default)]
    pub archive_password: Option<String>,
    #[serde(default = "default_true")]
    pub is_active: bool,
    /// Account password, required when `security.require_password_on_delete` is on.
    #[serde(default)]
    pub password: Option<String>,
}

fn default_retention() -> i64 {
    7
}
fn default_true() -> bool {
    true
}

fn lock(state: &SharedState) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
    state
        .db
        .lock()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))
}

/// Shared checks for create and update. Returns the encrypted archive
/// password when a new one was supplied.
fn validate(
    db: &Connection,
    user: &AuthUser,
    body: &ExportScheduleBody,
    password_required: bool,
) -> AppResult<Option<String>> {
    crate::api::security::verify_delete_password(db, &user.id, body.password.as_deref())?;
    if body.name.trim().is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.export_schedules.name_required",
        )));
    }
    if body.s3_storage_id.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.backups.s3_storage_required",
        )));
    }
    validate_cron(&body.cron_expression)?;
    if body.retention_count < 1 {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.export_schedules.retention_min",
        )));
    }
    if matches!(&body.project_ids, Some(ids) if ids.is_empty()) && !body.include_unassigned {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.export.nothing_selected",
        )));
    }
    let pw = body.archive_password.as_deref().unwrap_or("");
    if pw.is_empty() && !password_required {
        return Ok(None);
    }
    if pw.chars().count() < MIN_ARCHIVE_PASSWORD {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.export.archive_password_short",
            &[("n", &MIN_ARCHIVE_PASSWORD.to_string())],
        )));
    }
    let enc = crate::crypto::encrypt(pw, &crate::crypto::get_secret_key())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt archive password: {e}")))?;
    Ok(Some(enc))
}

fn project_ids_json(ids: &Option<Vec<String>>) -> Option<String> {
    ids.as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "[]".into()))
}

/// GET /api/v1/export-schedules
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = lock(&state)?;
    let mut stmt = db.prepare(
        "SELECT es.id, es.name, es.s3_storage_id, st.name, st.storage_type,
                COALESCE(u.cron_expression, es.cron_expression), es.retention_count,
                es.project_ids, es.include_unassigned, es.include_dumps, es.is_active,
                es.last_run_at, datetime(u.next_run_at), u.last_status
           FROM export_schedules es
           LEFT JOIN s3_storages st ON st.id = es.s3_storage_id
           LEFT JOIN schedules u ON u.id = 'sched-exp-' || es.id
          ORDER BY es.created_at",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |r| {
            let ids: Option<String> = r.get(7)?;
            let ids: Option<Vec<String>> = ids.and_then(|s| serde_json::from_str(&s).ok());
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "s3_storage_id": r.get::<_, String>(2)?,
                "storage_name": r.get::<_, Option<String>>(3)?,
                "storage_type": r.get::<_, Option<String>>(4)?,
                "cron_expression": r.get::<_, String>(5)?,
                "retention_count": r.get::<_, i64>(6)?,
                "project_ids": ids,
                "include_unassigned": r.get::<_, bool>(8)?,
                "include_dumps": r.get::<_, bool>(9)?,
                "is_active": r.get::<_, bool>(10)?,
                "last_run_at": r.get::<_, Option<String>>(11)?,
                "next_run_at": r.get::<_, Option<String>>(12)?,
                "last_status": r.get::<_, Option<String>>(13)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/export-schedules
pub async fn create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<ExportScheduleBody>,
) -> AppResult<impl IntoResponse> {
    let db = lock(&state)?;
    let enc = validate(&db, &user, &body, true)?.unwrap_or_default();
    let id = uuid::Uuid::new_v4().to_string();
    db.execute(
        "INSERT INTO export_schedules
            (id, name, s3_storage_id, cron_expression, retention_count, project_ids,
             include_unassigned, include_dumps, archive_password_enc, is_active)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.s3_storage_id,
            body.cron_expression,
            body.retention_count,
            project_ids_json(&body.project_ids),
            body.include_unassigned,
            body.include_dumps,
            enc,
            body.is_active,
        ],
    )?;
    sync_unified_for_export(
        &db,
        &id,
        body.name.trim(),
        &body.cron_expression,
        body.is_active,
    )
    .map_err(AppError::Internal)?;
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// PUT /api/v1/export-schedules/{id}
pub async fn update(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ExportScheduleBody>,
) -> AppResult<impl IntoResponse> {
    let db = lock(&state)?;
    let enc = validate(&db, &user, &body, false)?;
    let changed = db.execute(
        "UPDATE export_schedules SET
            name = ?1, s3_storage_id = ?2, cron_expression = ?3, retention_count = ?4,
            project_ids = ?5, include_unassigned = ?6, include_dumps = ?7,
            archive_password_enc = COALESCE(?8, archive_password_enc),
            is_active = ?9, updated_at = datetime('now')
          WHERE id = ?10",
        rusqlite::params![
            body.name.trim(),
            body.s3_storage_id,
            body.cron_expression,
            body.retention_count,
            project_ids_json(&body.project_ids),
            body.include_unassigned,
            body.include_dumps,
            enc,
            body.is_active,
            id,
        ],
    )?;
    if changed == 0 {
        return Err(AppError::NotFound(crate::i18n::te(
            "errors.export_schedules.not_found",
        )));
    }
    sync_unified_for_export(
        &db,
        &id,
        body.name.trim(),
        &body.cron_expression,
        body.is_active,
    )
    .map_err(AppError::Internal)?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/v1/export-schedules/{id}. Archives already uploaded stay in
/// the storage and in the run history (their `schedule_id` becomes NULL).
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = lock(&state)?;
    // The AFTER DELETE trigger (migration 73) drops the `schedules` mirror.
    db.execute("DELETE FROM export_schedules WHERE id = ?1", [&id])?;
    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/export-schedules/{id}/run — start one export in the
/// background; progress shows up in the run history.
pub async fn run_now(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let exists: bool = lock(&state)?.query_row(
        "SELECT EXISTS(SELECT 1 FROM export_schedules WHERE id = ?1)",
        [&id],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(AppError::NotFound(crate::i18n::te(
            "errors.export_schedules.not_found",
        )));
    }
    tokio::spawn(async move {
        if let Err(e) = scheduled::run_for_schedule(&state, &id, "manual").await {
            tracing::warn!("manual env export {id} failed: {e:#}");
        }
    });
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct RunsQuery {
    #[serde(default)]
    pub schedule_id: Option<String>,
}

/// GET /api/v1/export-runs?schedule_id=
pub async fn list_runs(
    State(state): State<SharedState>,
    Query(q): Query<RunsQuery>,
) -> AppResult<impl IntoResponse> {
    let db = lock(&state)?;
    let mut stmt = db.prepare(
        "SELECT r.id, r.schedule_id, es.name, r.s3_storage_id, st.name, r.s3_key, r.status,
                r.size_bytes, r.services_count, r.error_message, r.triggered_by,
                r.started_at, r.finished_at
           FROM export_runs r
           LEFT JOIN export_schedules es ON es.id = r.schedule_id
           LEFT JOIN s3_storages st ON st.id = r.s3_storage_id
          WHERE ?1 IS NULL OR r.schedule_id = ?1
          ORDER BY r.started_at DESC LIMIT 100",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([&q.schedule_id], |r| {
            Ok(serde_json::json!({
                "id": r.get::<_, String>(0)?,
                "schedule_id": r.get::<_, Option<String>>(1)?,
                "schedule_name": r.get::<_, Option<String>>(2)?,
                "s3_storage_id": r.get::<_, String>(3)?,
                "storage_name": r.get::<_, Option<String>>(4)?,
                "s3_key": r.get::<_, String>(5)?,
                "status": r.get::<_, String>(6)?,
                "size_bytes": r.get::<_, i64>(7)?,
                "services_count": r.get::<_, i64>(8)?,
                "error_message": r.get::<_, Option<String>>(9)?,
                "triggered_by": r.get::<_, String>(10)?,
                "started_at": r.get::<_, String>(11)?,
                "finished_at": r.get::<_, Option<String>>(12)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// Storage row + key of one export run.
fn load_run(db: &Connection, run_id: &str) -> AppResult<(scheduled::Storage, String, String)> {
    let (storage_id, key, status): (String, String, String) = db
        .query_row(
            "SELECT s3_storage_id, s3_key, status FROM export_runs WHERE id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te("errors.export_schedules.run_not_found"))
        })?;
    let storage = scheduled::load_storage(db, &storage_id)
        .map_err(|_| AppError::NotFound(crate::i18n::te("errors.export_schedules.storage_gone")))?;
    Ok((storage, key, status))
}

/// GET /api/v1/export-runs/{id}/download — the encrypted zip as stored.
pub async fn download_run(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (st, key, status) = load_run(&*lock(&state)?, &id)?;
    if status != "completed" {
        return Err(AppError::NotFound(crate::i18n::te(
            "errors.export_schedules.run_not_found",
        )));
    }
    let data = download_blob(
        &st.storage_type,
        &st.endpoint,
        &st.region,
        &st.bucket,
        &st.access_key,
        &st.secret_key,
        &key,
    )
    .await?;
    let filename = key.rsplit('/').next().unwrap_or("pier-export.zip");
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                "application/zip".to_string(),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (axum::http::header::CACHE_CONTROL, "no-store".to_string()),
        ],
        data,
    )
        .into_response())
}

/// DELETE /api/v1/export-runs/{id} — removes the archive from storage and
/// the run from history. A failed run has no blob, only the row goes.
pub async fn delete_run(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let loaded = load_run(&*lock(&state)?, &id);
    match loaded {
        Ok((st, key, status)) if status == "completed" => {
            crate::s3::delete_blob(
                &st.storage_type,
                &st.endpoint,
                &st.region,
                &st.bucket,
                &st.access_key,
                &st.secret_key,
                &key,
            )
            .await
            .map_err(AppError::Internal)?;
        }
        Ok((_, _, status)) if status == "running" => {
            return Err(AppError::Conflict(crate::i18n::te(
                "errors.export_schedules.run_in_progress",
            )));
        }
        Ok(_) => {}
        // Storage deleted since: nothing left to remove remotely.
        Err(AppError::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    lock(&state)?.execute("DELETE FROM export_runs WHERE id = ?1", [&id])?;
    Ok(Json(serde_json::json!({"ok": true})))
}
