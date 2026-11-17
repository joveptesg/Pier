use axum::extract::{Multipart, Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::Connection;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::backup::executor::DbCredential;
use crate::backup::scheduler::{build_s3_key, load_db_credentials};
use crate::error::{AppError, AppResult};
use crate::scheduler::cron_utils;
use crate::state::SharedState;

/// Body of POST /api/v1/resources/{id}/backup-schedules.
/// `database_name` is optional — omit for a cluster-wide schedule, set to a
/// specific DB name for a per-DB schedule. A database may have several
/// schedules (different cron / retention / storage); only an exact
/// duplicate (same database, storage and cron) is rejected.
#[derive(Deserialize)]
pub struct CreateScheduleRequest {
    pub s3_storage_id: String,
    #[serde(default = "default_cron")]
    pub cron_expression: String,
    #[serde(default = "default_retention")]
    pub retention_count: i64,
    #[serde(default)]
    pub database_name: Option<String>,
}

/// Body of PATCH /api/v1/resources/{id}/backup-schedules/{sid}.
#[derive(Deserialize)]
pub struct UpdateScheduleRequest {
    #[serde(default)]
    pub s3_storage_id: Option<String>,
    #[serde(default)]
    pub cron_expression: Option<String>,
    #[serde(default)]
    pub retention_count: Option<i64>,
    #[serde(default)]
    pub is_active: Option<bool>,
}

fn default_cron() -> String {
    "0 2 * * *".to_string()
}
fn default_retention() -> i64 {
    7
}

/// Reject a cron expression the unified scheduler could not run.
pub(crate) fn validate_cron(expr: &str) -> AppResult<()> {
    cron_utils::validate(expr).map_err(AppError::BadRequest)
}

/// Two schedules for the same database, storage and cron would just write
/// the same dump twice — that is the only combination we refuse.
fn ensure_not_duplicate(
    db: &Connection,
    service_id: &str,
    database_name: Option<&str>,
    s3_storage_id: &str,
    cron_expression: &str,
    exclude_id: Option<&str>,
) -> AppResult<()> {
    let dup: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM backup_schedules
          WHERE service_id = ?1
            AND COALESCE(database_name, '') = COALESCE(?2, '')
            AND s3_storage_id = ?3
            AND cron_expression = ?4
            AND id <> COALESCE(?5, ''))",
        rusqlite::params![
            service_id,
            database_name,
            s3_storage_id,
            cron_expression,
            exclude_id
        ],
        |r| r.get(0),
    )?;
    if dup {
        return Err(AppError::Conflict(crate::i18n::te(
            "errors.backups.duplicate_schedule",
        )));
    }
    Ok(())
}

/// Shared SELECT for the per-resource and the global schedule lists.
/// `cron_expression` / `next_run_at` / `last_status` come from the unified
/// `schedules` mirror — that is what the runner actually executes and
/// advances; `backup_schedules.next_run_at` is only an insert-time guess.
const SCHEDULE_SELECT: &str = "
    SELECT bs.id, bs.service_id, sv.name, p.name, sv.catalog_id,
           bs.s3_storage_id, st.name, st.storage_type,
           COALESCE(u.cron_expression, bs.cron_expression), bs.retention_count,
           bs.is_active, bs.last_run_at, datetime(u.next_run_at), u.last_status,
           bs.database_name
      FROM backup_schedules bs
      JOIN services sv ON sv.id = bs.service_id
      LEFT JOIN projects p ON p.id = sv.project_id
      LEFT JOIN s3_storages st ON st.id = bs.s3_storage_id
      LEFT JOIN schedules u ON u.id = 'sched-bk-' || bs.id";

fn schedule_row_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "service_id": row.get::<_, String>(1)?,
        "service_name": row.get::<_, String>(2)?,
        "project_name": row.get::<_, Option<String>>(3)?,
        "catalog_id": row.get::<_, Option<String>>(4)?,
        "s3_storage_id": row.get::<_, String>(5)?,
        "storage_name": row.get::<_, Option<String>>(6)?,
        "storage_type": row.get::<_, Option<String>>(7)?,
        "cron_expression": row.get::<_, String>(8)?,
        "retention_count": row.get::<_, i64>(9)?,
        "is_active": row.get::<_, bool>(10)?,
        "last_run_at": row.get::<_, Option<String>>(11)?,
        "next_run_at": row.get::<_, Option<String>>(12)?,
        "last_status": row.get::<_, Option<String>>(13)?,
        "database_name": row.get::<_, Option<String>>(14)?,
    }))
}

/// Shared SELECT for the per-resource and the global backup history.
const BACKUP_SELECT: &str = "
    SELECT b.id, b.status, b.size_bytes, b.error_message, b.triggered_by,
           b.started_at, b.finished_at, b.s3_key, b.database_name,
           b.schedule_id, b.s3_storage_id, st.name, b.service_id, sv.name
      FROM backups b
      LEFT JOIN s3_storages st ON st.id = b.s3_storage_id
      LEFT JOIN services sv ON sv.id = b.service_id";

fn backup_row_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<serde_json::Value> {
    Ok(serde_json::json!({
        "id": row.get::<_, String>(0)?,
        "status": row.get::<_, String>(1)?,
        "size_bytes": row.get::<_, i64>(2)?,
        "error_message": row.get::<_, Option<String>>(3)?,
        "triggered_by": row.get::<_, String>(4)?,
        "started_at": row.get::<_, String>(5)?,
        "finished_at": row.get::<_, Option<String>>(6)?,
        "s3_key": row.get::<_, String>(7)?,
        "database_name": row.get::<_, Option<String>>(8)?,
        "schedule_id": row.get::<_, Option<String>>(9)?,
        "s3_storage_id": row.get::<_, String>(10)?,
        "storage_name": row.get::<_, Option<String>>(11)?,
        "service_id": row.get::<_, String>(12)?,
        "service_name": row.get::<_, Option<String>>(13)?,
    }))
}

/// Stable id of the unified-scheduler row that mirrors a `backup_schedules`
/// entry. Matches the backfill pattern in migration 48, so existing rows
/// keep working without a second pass.
fn unified_id(backup_schedule_id: &str) -> String {
    format!("sched-bk-{backup_schedule_id}")
}

/// UPSERT the unified-scheduler row for a backup schedule. Called from the
/// API handlers whenever the legacy `backup_schedules` table changes so the
/// new runner sees an up-to-date `cron_expression` + `next_run_at`.
fn sync_unified_for_backup(
    db: &Connection,
    backup_schedule_id: &str,
    service_id: &str,
    cron_expression: &str,
    is_active: bool,
) -> anyhow::Result<()> {
    let id = unified_id(backup_schedule_id);
    let next = cron_utils::next_fire_utc(cron_expression, "UTC", chrono::Utc::now())
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());

    // Resolve a friendlier display name from the service row.
    let service_name: Option<String> = db
        .query_row(
            "SELECT name FROM services WHERE id = ?1",
            [service_id],
            |row| row.get(0),
        )
        .ok();
    // Per-DB schedules get a `(database_name)` suffix; cluster-wide rows
    // get `(cluster)`. Without this, services with multiple backup schedules
    // (cluster dump + N per-DB dumps) all collapse to the same label in
    // the unified Schedules list. Migration 50 rewrites legacy rows the
    // same way.
    let database_name: Option<String> = db
        .query_row(
            "SELECT database_name FROM backup_schedules WHERE id = ?1",
            [backup_schedule_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten();
    let suffix = match database_name.as_deref() {
        Some(name) if !name.is_empty() => format!(" ({name})"),
        _ => " (cluster)".to_string(),
    };
    let label = format!(
        "Backup: {}{suffix}",
        service_name.unwrap_or_else(|| service_id.to_string())
    );
    let config = format!("{{\"backup_schedule_id\":\"{backup_schedule_id}\"}}");

    // Replace the row outright on every update. Cheaper than an
    // emulated UPSERT path and keeps the audit-style created_by /
    // is_system / description columns from the migration unchanged.
    db.execute(
        "INSERT INTO schedules
            (id, name, cron_expression, timezone, action_type, action_config,
             enabled, is_system, next_run_at)
         VALUES (?1, ?2, ?3, 'UTC', 'backup', ?4, ?5, 1, ?6)
         ON CONFLICT(id) DO UPDATE SET
             name            = excluded.name,
             cron_expression = excluded.cron_expression,
             action_config   = excluded.action_config,
             enabled         = excluded.enabled,
             next_run_at     = excluded.next_run_at,
             updated_at      = datetime('now')",
        rusqlite::params![id, label, cron_expression, config, is_active as i64, next,],
    )?;
    Ok(())
}

/// Drop the mirror row when a `backup_schedules` row is deleted.
fn drop_unified_for_backup(db: &Connection, backup_schedule_id: &str) -> anyhow::Result<()> {
    let id = unified_id(backup_schedule_id);
    db.execute("DELETE FROM schedules WHERE id = ?1", [&id])?;
    Ok(())
}

/// GET /api/v1/resources/{id}/backup-schedules
/// Returns all schedules for the service, cluster-wide first then per-DB.
pub async fn list_schedules(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(&format!(
        "{SCHEDULE_SELECT}
         WHERE bs.service_id = ?1
         ORDER BY (bs.database_name IS NOT NULL), bs.database_name, bs.created_at"
    ))?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([&id], schedule_row_json)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// GET /api/v1/backup-schedules — every backup schedule of every service
/// (System → Backups). Admin only.
pub async fn list_all_schedules(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(&format!(
        "{SCHEDULE_SELECT}
         ORDER BY sv.name, (bs.database_name IS NOT NULL), bs.database_name, bs.created_at"
    ))?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], schedule_row_json)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/resources/{id}/backup-schedules
/// Always inserts a new schedule; editing goes through PATCH.
pub async fn create_schedule(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<CreateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;
    if body.s3_storage_id.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.backups.s3_storage_required",
        )));
    }
    validate_cron(&body.cron_expression)?;
    let database_name = body.database_name.filter(|n| !n.is_empty());

    let schedule_id = uuid::Uuid::new_v4().to_string();

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    ensure_not_duplicate(
        &db,
        &id,
        database_name.as_deref(),
        &body.s3_storage_id,
        &body.cron_expression,
        None,
    )?;

    db.execute(
        "INSERT INTO backup_schedules
           (id, service_id, s3_storage_id, cron_expression, retention_count, database_name)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            schedule_id,
            id,
            body.s3_storage_id,
            body.cron_expression,
            body.retention_count,
            database_name
        ],
    )?;

    sync_unified_for_backup(&db, &schedule_id, &id, &body.cron_expression, true)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("sync scheduler row: {e}")))?;

    Ok(Json(serde_json::json!({"ok": true, "id": schedule_id})))
}

/// PATCH /api/v1/resources/{id}/backup-schedules/{sid}
pub async fn update_schedule(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, schedule_id)): Path<(String, String)>,
    Json(body): Json<UpdateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;
    if let Some(v) = &body.cron_expression {
        validate_cron(v)?;
    }
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // The schedule must belong to the resource the caller was authorised for.
    let (database_name, cur_storage, cur_cron): (Option<String>, String, String) = db
        .query_row(
            "SELECT database_name, s3_storage_id, cron_expression
               FROM backup_schedules WHERE id = ?1 AND service_id = ?2",
            [&schedule_id, &id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| AppError::NotFound(crate::i18n::te("errors.backups.schedule_not_found")))?;
    ensure_not_duplicate(
        &db,
        &id,
        database_name.as_deref(),
        body.s3_storage_id.as_deref().unwrap_or(&cur_storage),
        body.cron_expression.as_deref().unwrap_or(&cur_cron),
        Some(&schedule_id),
    )?;

    if let Some(v) = &body.s3_storage_id {
        db.execute(
            "UPDATE backup_schedules SET s3_storage_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v, schedule_id],
        )?;
    }
    if let Some(v) = &body.cron_expression {
        db.execute(
            "UPDATE backup_schedules SET cron_expression = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v, schedule_id],
        )?;
    }
    if let Some(v) = body.retention_count {
        db.execute(
            "UPDATE backup_schedules SET retention_count = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v, schedule_id],
        )?;
    }
    if let Some(v) = body.is_active {
        db.execute(
            "UPDATE backup_schedules SET is_active = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v as i64, schedule_id],
        )?;
    }

    // Sync the mirror row. We re-read the canonical fields after the
    // partial update so we don't have to recompute the merged state in
    // application code.
    let (service_id, cron_expr, active): (String, String, bool) = db.query_row(
        "SELECT service_id, cron_expression, is_active FROM backup_schedules WHERE id = ?1",
        [&schedule_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    sync_unified_for_backup(&db, &schedule_id, &service_id, &cron_expr, active)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("sync scheduler row: {e}")))?;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/v1/resources/{id}/backup-schedules/{sid}
pub async fn delete_schedule(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, schedule_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let deleted = db.execute(
        "DELETE FROM backup_schedules WHERE id = ?1 AND service_id = ?2",
        [&schedule_id, &id],
    )?;
    // Not this resource's rule: leave it and its scheduler mirror alone.
    if deleted == 0 {
        return Err(AppError::NotFound(crate::i18n::te(
            "errors.backups.schedule_not_found",
        )));
    }
    drop_unified_for_backup(&db, &schedule_id)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("drop scheduler row: {e}")))?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize, Default)]
pub struct TriggerQuery {
    /// Optional DB name. Omit for cluster-wide backup.
    #[serde(default)]
    pub database_name: Option<String>,
    /// Run this exact schedule (its database and storage). Takes precedence
    /// over `database_name` — a database may have several schedules.
    #[serde(default)]
    pub schedule_id: Option<String>,
}

/// POST /api/v1/resources/{id}/backups/trigger?database_name=...|schedule_id=...
///
/// With `schedule_id`, backs up that schedule's database to its storage.
/// Otherwise picks the storage from a schedule matching the target (per-DB
/// or cluster-wide), falling back to any active schedule of the service so
/// users can trigger an ad-hoc per-DB backup without a per-DB schedule.
pub async fn trigger_backup(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(mut q): Query<TriggerQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    // An explicit schedule pins both the database and the storage.
    let pinned: Option<(String, String)> = match q.schedule_id.as_deref() {
        Some(sid) => {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            let (storage, db_name): (String, Option<String>) = db
                .query_row(
                    "SELECT s3_storage_id, database_name FROM backup_schedules
                      WHERE id = ?1 AND service_id = ?2",
                    [sid, &id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|_| {
                    AppError::NotFound(crate::i18n::te("errors.backups.schedule_not_found"))
                })?;
            q.database_name = db_name;
            Some((storage, sid.to_string()))
        }
        None => None,
    };
    let (name, catalog_id, env_json) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id, env_json FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.backups.resource_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let catalog_id_str = catalog_id.unwrap_or_default();
    if !crate::backup::executor::supports_backup(&catalog_id_str) {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.backup_not_supported",
            &[("catalog", &catalog_id_str)],
        )));
    }
    if q.database_name.is_some()
        && !crate::backup::executor::supports_per_db_backup(&catalog_id_str)
    {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.per_db_backup_not_supported",
            &[("catalog", &catalog_id_str)],
        )));
    }

    // Resolve schedule → s3_storage_id. First try exact match on
    // (service, database_name); fall back to the cluster-wide schedule.
    let (s3_storage_id, schedule_id) = if let Some(pair) = pinned {
        pair
    } else {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let exact: Option<(String, String)> = db
            .query_row(
                "SELECT s3_storage_id, id FROM backup_schedules
                 WHERE service_id = ?1
                   AND COALESCE(database_name, '') = COALESCE(?2, '')
                   AND is_active = 1
                 ORDER BY created_at LIMIT 1",
                rusqlite::params![id, q.database_name],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .ok();
        if let Some(pair) = exact {
            pair
        } else {
            // Fallback: any active schedule for this service.
            db.query_row(
                "SELECT s3_storage_id, id FROM backup_schedules
                 WHERE service_id = ?1 AND is_active = 1
                 ORDER BY (database_name IS NOT NULL) LIMIT 1",
                [&id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| {
                AppError::BadRequest(crate::i18n::te("errors.backups.no_schedule_configured"))
            })?
        }
    };

    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: std::collections::HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();

    let (storage_type, endpoint, region, bucket, access_key, secret_key, key_prefix) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key, key_prefix FROM s3_storages WHERE id = ?1",
            [&s3_storage_id],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            )),
        )?
    };

    let backup_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let s3_key = build_s3_key(
        &key_prefix,
        &name,
        &catalog_id_str,
        q.database_name.as_deref(),
        &timestamp,
    );

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO backups (id, schedule_id, service_id, s3_storage_id, s3_key, database_name, status, triggered_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', 'manual')",
            rusqlite::params![backup_id, schedule_id, id, s3_storage_id, s3_key, q.database_name],
        )?;
    }

    // Snapshot inputs for the async task.
    let bid = backup_id.clone();
    let state2 = state.clone();
    let service_id_for_creds = id.clone();
    let db_name_opt = q.database_name.clone();
    // Run of one specific rule (its Run button) — counts as that rule's run.
    let ran_schedule = q.schedule_id.clone();

    tokio::spawn(async move {
        let container_name = format!("pier-{name}");
        let dump_result = match &db_name_opt {
            Some(db_name) => {
                let creds = match load_db_credentials(&state2, &service_id_for_creds) {
                    Ok(c) => c,
                    Err(e) => {
                        mark_failed(&state2, &bid, &format!("load credentials: {e}"));
                        return;
                    }
                };
                let cred = creds.iter().find(|c| &c.db_name == db_name);
                crate::backup::executor::execute_db_backup(
                    &container_name,
                    &catalog_id_str,
                    &env_vars,
                    cred,
                    db_name,
                )
                .await
            }
            None => {
                let creds = match load_db_credentials(&state2, &service_id_for_creds) {
                    Ok(c) => c,
                    Err(e) => {
                        mark_failed(&state2, &bid, &format!("load credentials: {e}"));
                        return;
                    }
                };
                crate::backup::executor::execute_cluster_backup(
                    &container_name,
                    &catalog_id_str,
                    &env_vars,
                    &creds,
                )
                .await
            }
        };

        let data = match dump_result {
            Ok(d) => d,
            Err(e) => {
                mark_failed(&state2, &bid, &e.to_string());
                return;
            }
        };

        let size = data.len() as i64;

        let upload_result = match storage_type.as_str() {
            "bunny" => {
                crate::s3::bunny::upload_file(&bucket, &access_key, &endpoint, &s3_key, data).await
            }
            _ => match crate::s3::build_client(&endpoint, &region, &access_key, &secret_key) {
                Ok(client) => crate::s3::upload_file(&client, &bucket, &s3_key, data).await,
                Err(e) => Err(e),
            },
        };

        if let Ok(db) = state2.db.lock() {
            match upload_result {
                Ok(_) => {
                    let _ = db.execute(
                        "UPDATE backups SET status='completed', size_bytes=?1, finished_at=datetime('now') WHERE id=?2",
                        rusqlite::params![size, bid],
                    );
                    if let Some(sid) = &ran_schedule {
                        let _ = db.execute(
                            "UPDATE backup_schedules SET last_run_at = datetime('now') WHERE id = ?1",
                            [sid],
                        );
                    }
                }
                Err(e) => {
                    let _ = db.execute(
                        "UPDATE backups SET status='failed', error_message=?1, finished_at=datetime('now') WHERE id=?2",
                        rusqlite::params![e.to_string(), bid],
                    );
                }
            }
        }
    });

    Ok(Json(serde_json::json!({"ok": true, "id": backup_id})))
}

fn mark_failed(state: &SharedState, backup_id: &str, msg: &str) {
    if let Ok(db) = state.db.lock() {
        let _ = db.execute(
            "UPDATE backups SET status='failed', error_message=?1, finished_at=datetime('now') WHERE id=?2",
            rusqlite::params![msg, backup_id],
        );
    }
}

/// GET /api/v1/resources/{id}/backups
pub async fn list_backups(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(&format!(
        "{BACKUP_SELECT} WHERE b.service_id = ?1 ORDER BY b.started_at DESC LIMIT 50"
    ))?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([&id], backup_row_json)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

#[derive(Deserialize)]
pub struct ListAllBackupsQuery {
    #[serde(default = "default_history_limit")]
    pub limit: i64,
}

fn default_history_limit() -> i64 {
    100
}

/// GET /api/v1/backups?limit=N — latest backups across every service
/// (System → Backups). Admin only.
pub async fn list_all_backups(
    State(state): State<SharedState>,
    Query(q): Query<ListAllBackupsQuery>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(&format!(
        "{BACKUP_SELECT} ORDER BY b.started_at DESC LIMIT ?1"
    ))?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([q.limit.clamp(1, 500)], backup_row_json)?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// Body of POST /api/v1/backups/{backup_id}/restore.
#[derive(Deserialize)]
pub struct RestoreRequest {
    /// Name of the database to restore into. MUST already exist in the
    /// service's `database_credentials` — the restore drops and recreates
    /// the target DB using that row's owner/password.
    pub target_database_name: String,
}

/// POST /api/v1/backups/{backup_id}/restore
///
/// Downloads the backup from S3, if it's a cluster-wide tar archive extracts
/// the requested per-DB SQL file from it, then drops+recreates the target
/// database and streams the dump in. Per-DB only — cluster-wide restore is
/// deliberately not supported (see plan, "Восстановление только per-DB").
pub async fn restore_backup(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(backup_id): Path<String>,
    Json(body): Json<RestoreRequest>,
) -> AppResult<impl IntoResponse> {
    // Resolve service_id from the backup row, then gate on its project as Admin
    // (restore is destructive — it drops & recreates the target DB).
    let service_id: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT service_id FROM backups WHERE id = ?1",
            [&backup_id],
            |r| r.get(0),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.backups.backup_not_found",
                &[("id", &backup_id)],
            ))
        })?
    };
    enforce_resource_role(&state, &user, &service_id, ProjectRole::Admin)?;

    if body.target_database_name.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.backups.target_database_name_required",
        )));
    }

    // 1. Resolve backup → service + storage + (cluster/per-DB) flag.
    let (service_id, s3_storage_id, s3_key, backup_db_name) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT service_id, s3_storage_id, s3_key, database_name FROM backups
             WHERE id = ?1 AND status = 'completed'",
            [&backup_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(crate::i18n::te("errors.backups.backup_not_completed")))?
    };

    // 2. Service info + catalog gate.
    let (name, catalog_id, env_json) = fetch_service_info(&state, &service_id)?;
    if !crate::backup::executor::supports_per_db_backup(&catalog_id) {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.per_db_restore_not_supported",
            &[("catalog", &catalog_id)],
        )));
    }

    // 3. Owner credentials for the target DB — must already exist.
    let owner = resolve_owner(&state, &service_id, &body.target_database_name)?;

    // 4. S3 config + download blob.
    let (storage_type, endpoint, region, bucket, access_key, secret_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key FROM s3_storages WHERE id = ?1",
            [&s3_storage_id],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            )),
        )?
    };

    let blob = download_blob(
        &storage_type,
        &endpoint,
        &region,
        &bucket,
        &access_key,
        &secret_key,
        &s3_key,
    )
    .await?;

    // 5. If per-DB backup doesn't match the restore target, bail — avoids
    //    restoring 'bar'.sql into a DB named 'foo'. Applies to SQL *and* Mongo
    //    per-DB archives.
    if let Some(db) = backup_db_name.as_deref() {
        if db != body.target_database_name {
            return Err(AppError::BadRequest(crate::i18n::te_args(
                "errors.backups.backup_db_mismatch",
                &[("backup_db", db), ("target_db", &body.target_database_name)],
            )));
        }
    }

    // 6. Decrypt env + container name + format detection from the s3_key
    //    suffix, then dispatch through the shared `apply_restore` path.
    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: std::collections::HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();
    let container_name = format!("pier-{name}");
    let fmt = UploadFormat {
        is_gzipped: crate::backup::restore::is_gzipped(&s3_key),
        is_cluster_archive: crate::backup::restore::is_cluster_archive(&s3_key),
        is_pg_custom: crate::backup::restore::is_pg_custom_format(&s3_key),
    };

    apply_restore(
        &container_name,
        &catalog_id,
        &env_vars,
        &body.target_database_name,
        &owner,
        fmt,
        blob,
    )
    .await?;

    crate::alerts::hooks::fire_event(
        &state,
        "backup_status",
        Some(&service_id),
        format!(
            "Restored '{}' on {name} from backup {backup_id}",
            body.target_database_name
        ),
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/resources/{id}/databases/{dbname}/restore-upload
///
/// Multipart endpoint that restores `dbname` from a backup file uploaded
/// directly by the user — used for cross-cluster disaster recovery, where
/// the user has manually downloaded a `.dump` (or `.sql.gz` / `.tar.gz` /
/// `.archive.gz`) and wants to replay it on a fresh Pier instance that has
/// no S3 storage configured.
///
/// The target DB must already exist on the service (created via the
/// Databases UI) — its `database_credentials` row supplies the owner role
/// and password used by `drop_and_recreate_pg_db` for role-sync.
///
/// Format is detected from the uploaded filename. Cluster-wide tars are
/// supported the same way as in `restore_backup`: per-DB extraction by
/// matching `<dbname>.dump` or `<dbname>.sql` entries.
pub async fn restore_database_from_upload(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dbname)): Path<(String, String)>,
    mut multipart: Multipart,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;
    if dbname.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.backups.database_name_required",
        )));
    }

    let (name, catalog_id, env_json) = fetch_service_info(&state, &id)?;
    if !crate::backup::executor::supports_per_db_backup(&catalog_id) {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.per_db_restore_not_supported",
            &[("catalog", &catalog_id)],
        )));
    }

    let owner = resolve_owner(&state, &id, &dbname)?;

    // Read multipart: locate the `file` field. We accept it in any position
    // (clients may put a metadata field before or after).
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;
    while let Some(field) = multipart.next_field().await.map_err(|e| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.multipart_read_error",
            &[("error", &e.to_string())],
        ))
    })? {
        if field.name() == Some("file") {
            file_name = field.file_name().map(|s| s.to_string());
            let bytes = field.bytes().await.map_err(|e| {
                AppError::BadRequest(crate::i18n::te_args(
                    "errors.backups.file_read_error",
                    &[("error", &e.to_string())],
                ))
            })?;
            file_bytes = Some(bytes.to_vec());
        }
    }
    let blob = file_bytes.ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te("errors.backups.multipart_missing_file"))
    })?;
    let filename = file_name.ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te("errors.backups.uploaded_file_no_name"))
    })?;

    let fmt = detect_format(&filename, &catalog_id)?;

    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: std::collections::HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();
    let container_name = format!("pier-{name}");

    apply_restore(
        &container_name,
        &catalog_id,
        &env_vars,
        &dbname,
        &owner,
        fmt,
        blob,
    )
    .await?;

    crate::alerts::hooks::fire_event(
        &state,
        "backup_status",
        Some(&id),
        format!("Restored '{dbname}' on {name} from uploaded file '{filename}'"),
    )
    .await;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// Wire format of the dump payload, derived either from an S3 key suffix
/// (`restore_backup`) or from an uploaded filename (`restore_database_from_upload`).
/// Drives the restore dispatcher in `apply_restore`. Mongo vs SQL is decided
/// from `catalog_id` (Mongo archives only ever come from a Mongo catalog —
/// `detect_format` enforces that), so there is no `is_mongo_archive` field
/// here.
#[derive(Debug, Clone, Copy)]
struct UploadFormat {
    /// Outer gzip wrapper present (`.gz` suffix). Decompressed in Rust before
    /// further processing — except for Mongo archives, which are handed to
    /// mongorestore with `--gzip` for in-place decompression.
    is_gzipped: bool,
    /// Cluster-wide tar archive (`.tar` / `.tar.gz`) — needs per-DB extraction.
    is_cluster_archive: bool,
    /// PostgreSQL custom format (`.dump`, `pg_dump -Fc`) — restored via
    /// `pg_restore`. False for plain SQL (legacy `.sql.gz`, MySQL).
    is_pg_custom: bool,
}

fn fetch_service_info(
    state: &SharedState,
    service_id: &str,
) -> AppResult<(String, String, Option<String>)> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let (name, catalog_id, env_json) = db
        .query_row(
            "SELECT name, catalog_id, env_json FROM services WHERE id = ?1",
            [service_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.backups.service_not_found",
                &[("id", service_id)],
            ))
        })?;
    Ok((name, catalog_id.unwrap_or_default(), env_json))
}

fn resolve_owner(
    state: &SharedState,
    service_id: &str,
    target_db: &str,
) -> AppResult<DbCredential> {
    let creds = load_db_credentials(state, service_id)?;
    creds
        .into_iter()
        .find(|c| c.db_name == target_db)
        .ok_or_else(|| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.backups.database_does_not_exist",
                &[("name", target_db)],
            ))
        })
}

/// Validate that the given filename looks like a backup whose format matches
/// the target database engine, and decode it into an `UploadFormat`. The
/// extension check uses the same suffix-based logic that `restore.rs` uses
/// for S3 keys, so detection stays in sync between the two restore paths.
fn detect_format(filename: &str, catalog_id: &str) -> AppResult<UploadFormat> {
    let name = filename.to_lowercase();

    let is_gzipped = name.ends_with(".gz");
    let is_cluster_archive = name.ends_with(".tar") || name.ends_with(".tar.gz");
    let is_pg_custom = name.ends_with(".dump");
    let is_mongo_archive = name.ends_with(".archive") || name.ends_with(".archive.gz");
    let is_plain_sql = name.ends_with(".sql") || name.ends_with(".sql.gz");

    let recognized = is_pg_custom || is_cluster_archive || is_mongo_archive || is_plain_sql;
    if !recognized {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.unrecognized_file_extension",
            &[("filename", filename)],
        )));
    }

    // Catalog ↔ format compatibility. `.archive*` is Mongo-only, `.dump` is
    // Postgres-only; tarballs and plain SQL are shared between Postgres and
    // MySQL. We do NOT inspect file content here — extension is the contract.
    let compatible = match catalog_id {
        "postgresql" | "postgis" | "timescaledb" => {
            is_pg_custom || is_plain_sql || is_cluster_archive
        }
        "mysql" | "mariadb" => is_plain_sql || is_cluster_archive,
        "mongodb" => is_mongo_archive,
        _ => false,
    };
    if !compatible {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.backups.file_not_compatible",
            &[("filename", filename), ("catalog", catalog_id)],
        )));
    }

    Ok(UploadFormat {
        is_gzipped,
        is_cluster_archive,
        is_pg_custom,
    })
}

/// Shared dispatcher: take a downloaded/uploaded backup blob plus its
/// detected format and apply the appropriate restore path
/// (`execute_mongo_restore` for Mongo, otherwise gunzip → tar-extract →
/// `execute_restore`). Used by both `restore_backup` (S3 source) and
/// `restore_database_from_upload` (multipart source).
async fn apply_restore(
    container_name: &str,
    catalog_id: &str,
    env_vars: &std::collections::HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    fmt: UploadFormat,
    blob: Vec<u8>,
) -> AppResult<()> {
    if catalog_id == "mongodb" {
        // Mongo archives are binary BSON — no tar extraction. `--nsInclude`
        // filters the target DB whether the archive is full-instance or
        // per-DB. Gzipped archives are handed to mongorestore with `--gzip`
        // so it decompresses on the fly.
        return crate::backup::restore::execute_mongo_restore(
            container_name,
            env_vars,
            target_db,
            fmt.is_gzipped,
            blob,
        )
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("restore failed: {e}")));
    }

    // SQL path: gunzip (if outer .gz wrapper) → tar-extract (if cluster) →
    // pg_restore / psql. Plain `.sql` / `.tar` (no `.gz`) bypass gunzip so
    // legacy backups keep working. PostgreSQL `.dump` blobs carry their own
    // internal zlib — pg_restore handles it, no Rust-side gunzip needed.
    let unpacked = if fmt.is_gzipped {
        crate::backup::restore::gunzip_bytes(&blob)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("gunzip: {e}")))?
    } else {
        blob
    };
    let (is_pg_custom, dump_bytes) = if fmt.is_cluster_archive {
        crate::backup::restore::extract_db_from_tar(&unpacked, target_db)
            .map_err(|e| AppError::BadRequest(e.to_string()))?
    } else {
        (fmt.is_pg_custom, unpacked)
    };
    crate::backup::restore::execute_restore(
        container_name,
        catalog_id,
        env_vars,
        target_db,
        owner,
        is_pg_custom,
        dump_bytes,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("restore failed: {e}")))
}

pub(crate) async fn download_blob(
    storage_type: &str,
    endpoint: &str,
    region: &str,
    bucket: &str,
    access_key: &str,
    _secret_key: &str,
    s3_key: &str,
) -> AppResult<Vec<u8>> {
    match storage_type {
        "bunny" => {
            // Bunny Storage: GET https://{endpoint}/{bucket}/{key} with AccessKey header.
            let url = format!("https://{endpoint}/{bucket}/{s3_key}");
            let resp = reqwest::Client::new()
                .get(&url)
                .header("AccessKey", access_key)
                .send()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Bunny GET: {e}")))?;
            if !resp.status().is_success() {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "Bunny GET {url}: HTTP {}",
                    resp.status()
                )));
            }
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("Bunny read: {e}")))?;
            Ok(bytes.to_vec())
        }
        _ => {
            let client = crate::s3::build_client(endpoint, region, access_key, _secret_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 client: {e}")))?;
            let resp = client
                .get_object()
                .bucket(bucket)
                .key(s3_key)
                .send()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 download: {e}")))?;
            let body = resp
                .body
                .collect()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 read: {e}")))?;
            Ok(body.into_bytes().to_vec())
        }
    }
}

/// GET /api/v1/backups/{backup_id}/download
pub async fn download_backup(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(backup_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    // Resolve the owning service so we can gate on the project. Viewer is
    // enough: download lets you read a dump you already had read access to.
    let service_for_check: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT service_id FROM backups WHERE id = ?1",
            [&backup_id],
            |r| r.get(0),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.backups.backup_not_found",
                &[("id", &backup_id)],
            ))
        })?
    };
    enforce_resource_role(&state, &user, &service_for_check, ProjectRole::Viewer)?;
    let (s3_storage_id, s3_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT s3_storage_id, s3_key FROM backups WHERE id = ?1 AND status = 'completed'",
            [&backup_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| AppError::NotFound(crate::i18n::te("errors.backups.backup_not_completed")))?
    };

    let (storage_type, endpoint, region, bucket, access_key, secret_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key FROM s3_storages WHERE id = ?1",
            [&s3_storage_id],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            )),
        )?
    };

    match storage_type.as_str() {
        "bunny" => {
            let url = format!("https://{endpoint}/{bucket}/{s3_key}");
            Ok(axum::response::Redirect::temporary(&url).into_response())
        }
        _ => {
            let client = crate::s3::build_client(&endpoint, &region, &access_key, &secret_key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 client: {e}")))?;

            let resp = client
                .get_object()
                .bucket(&bucket)
                .key(&s3_key)
                .send()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 download: {e}")))?;

            let body = resp
                .body
                .collect()
                .await
                .map_err(|e| AppError::Internal(anyhow::anyhow!("S3 read: {e}")))?;

            let filename = s3_key.rsplit('/').next().unwrap_or("backup.dump");
            Ok((
                [
                    (
                        axum::http::header::CONTENT_TYPE,
                        "application/octet-stream".to_string(),
                    ),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                body.into_bytes().to_vec(),
            )
                .into_response())
        }
    }
}

/// DELETE /api/v1/backups/{backup_id}
///
/// Hard-deletes a backup from both object storage and the `backups` table.
/// Storage errors are surfaced (so callers know the blob might be orphaned);
/// if the row still needs removal after a storage failure, run this again
/// once the storage issue is resolved — it's idempotent on the Bunny side
/// (404 is treated as success) and on the S3 side (DeleteObject tolerates
/// missing keys).
pub async fn delete_backup(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(backup_id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let service_for_check: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT service_id FROM backups WHERE id = ?1",
            [&backup_id],
            |r| r.get(0),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.backups.backup_not_found",
                &[("id", &backup_id)],
            ))
        })?
    };
    enforce_resource_role(&state, &user, &service_for_check, ProjectRole::Admin)?;
    let (s3_storage_id, s3_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT s3_storage_id, s3_key FROM backups WHERE id = ?1",
            [&backup_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te("errors.backups.backup_not_found_simple"))
        })?
    };

    let (storage_type, endpoint, region, bucket, access_key, secret_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key FROM s3_storages WHERE id = ?1",
            [&s3_storage_id],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            )),
        )?
    };

    crate::s3::delete_blob(
        &storage_type,
        &endpoint,
        &region,
        &bucket,
        &access_key,
        &secret_key,
        &s3_key,
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("storage delete: {e}")))?;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute("DELETE FROM backups WHERE id = ?1", [&backup_id])?;
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_pg_custom_dump() {
        let f = detect_format("backup_2026-05-04.dump", "postgresql").unwrap();
        assert!(f.is_pg_custom);
        assert!(!f.is_gzipped);
        assert!(!f.is_cluster_archive);
    }

    #[test]
    fn detect_pg_custom_dump_for_postgis() {
        let f = detect_format("places.dump", "postgis").unwrap();
        assert!(f.is_pg_custom);
    }

    #[test]
    fn detect_legacy_sql_gz() {
        let f = detect_format("db_app_20260504.sql.gz", "postgresql").unwrap();
        assert!(!f.is_pg_custom);
        assert!(f.is_gzipped);
        assert!(!f.is_cluster_archive);
    }

    #[test]
    fn detect_cluster_tar_gz() {
        let f = detect_format("_cluster_20260504.tar.gz", "postgis").unwrap();
        assert!(f.is_cluster_archive);
        assert!(f.is_gzipped);
        assert!(!f.is_pg_custom);
    }

    #[test]
    fn detect_mongo_archive_gzipped() {
        // Mongo path is selected by catalog_id in apply_restore, not by a
        // dedicated UploadFormat flag; we only verify gzip detection here.
        let f = detect_format("users_20260504.archive.gz", "mongodb").unwrap();
        assert!(f.is_gzipped);
        assert!(!f.is_pg_custom);
        assert!(!f.is_cluster_archive);
    }

    #[test]
    fn detect_rejects_pg_dump_into_mysql() {
        let err = detect_format("foo.dump", "mysql").unwrap_err();
        assert!(format!("{err}").contains("not compatible"));
    }

    #[test]
    fn detect_rejects_mongo_archive_into_postgres() {
        let err = detect_format("foo.archive.gz", "postgresql").unwrap_err();
        assert!(format!("{err}").contains("not compatible"));
    }

    #[test]
    fn detect_rejects_unknown_extension() {
        let err = detect_format("foo.bin", "postgresql").unwrap_err();
        assert!(format!("{err}").contains("unrecognized"));
    }

    #[test]
    fn detect_case_insensitive() {
        // Browsers may upload with original casing on Windows; make sure we
        // don't reject `.DUMP` because of capitalization.
        let f = detect_format("MyBackup.DUMP", "postgresql").unwrap();
        assert!(f.is_pg_custom);
    }

    #[test]
    fn detect_mysql_accepts_plain_sql_gz() {
        let f = detect_format("dump.sql.gz", "mysql").unwrap();
        assert!(f.is_gzipped);
        assert!(!f.is_pg_custom);
    }
}
