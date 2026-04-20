use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::backup::scheduler::{build_s3_key, load_db_credentials};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// Body of POST /api/v1/resources/{id}/backup-schedules.
/// `database_name` is optional — omit for a cluster-wide schedule, set to a
/// specific DB name for a per-DB schedule. At most one schedule per
/// (service_id, database_name) pair is allowed (enforced by UNIQUE index).
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

fn cron_to_next_run(cron: &str) -> &'static str {
    match cron {
        "0 2 * * *" => "+1 day",
        "0 2 * * 0" => "+7 days",
        "0 */6 * * *" => "+6 hours",
        "0 * * * *" => "+1 hour",
        _ => "+1 day",
    }
}

/// GET /api/v1/resources/{id}/backup-schedules
/// Returns all schedules for the service, cluster-wide first then per-DB.
pub async fn list_schedules(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, s3_storage_id, cron_expression, retention_count, is_active,
                last_run_at, next_run_at, database_name
         FROM backup_schedules
         WHERE service_id = ?1
         ORDER BY (database_name IS NOT NULL), database_name",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "s3_storage_id": row.get::<_, String>(1)?,
                "cron_expression": row.get::<_, String>(2)?,
                "retention_count": row.get::<_, i64>(3)?,
                "is_active": row.get::<_, bool>(4)?,
                "last_run_at": row.get::<_, Option<String>>(5)?,
                "next_run_at": row.get::<_, Option<String>>(6)?,
                "database_name": row.get::<_, Option<String>>(7)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/resources/{id}/backup-schedules
/// Create-or-replace semantics: if a schedule already exists for the same
/// (service_id, database_name) pair, it is replaced.
pub async fn create_schedule(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<CreateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    if body.s3_storage_id.is_empty() {
        return Err(AppError::BadRequest("S3 storage is required".into()));
    }

    let schedule_id = uuid::Uuid::new_v4().to_string();
    let next_run = cron_to_next_run(&body.cron_expression);

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Replace any existing schedule for the same (service, database_name).
    db.execute(
        "DELETE FROM backup_schedules WHERE service_id = ?1
         AND COALESCE(database_name, '') = COALESCE(?2, '')",
        rusqlite::params![id, body.database_name],
    )?;

    db.execute(
        "INSERT INTO backup_schedules
           (id, service_id, s3_storage_id, cron_expression, retention_count, database_name, next_run_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now', ?7))",
        rusqlite::params![
            schedule_id,
            id,
            body.s3_storage_id,
            body.cron_expression,
            body.retention_count,
            body.database_name,
            next_run
        ],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": schedule_id})))
}

/// PATCH /api/v1/resources/{id}/backup-schedules/{sid}
pub async fn update_schedule(
    State(state): State<SharedState>,
    Path((_, schedule_id)): Path<(String, String)>,
    Json(body): Json<UpdateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    if let Some(v) = &body.s3_storage_id {
        db.execute(
            "UPDATE backup_schedules SET s3_storage_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![v, schedule_id],
        )?;
    }
    if let Some(v) = &body.cron_expression {
        db.execute(
            "UPDATE backup_schedules SET cron_expression = ?1, next_run_at = datetime('now', ?2), updated_at = datetime('now') WHERE id = ?3",
            rusqlite::params![v, cron_to_next_run(v), schedule_id],
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
    Ok(Json(serde_json::json!({"ok": true})))
}

/// DELETE /api/v1/resources/{id}/backup-schedules/{sid}
pub async fn delete_schedule(
    State(state): State<SharedState>,
    Path((_, schedule_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute("DELETE FROM backup_schedules WHERE id = ?1", [&schedule_id])?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize, Default)]
pub struct TriggerQuery {
    /// Optional DB name. Omit for cluster-wide backup.
    #[serde(default)]
    pub database_name: Option<String>,
}

/// POST /api/v1/resources/{id}/backups/trigger?database_name=...
///
/// Picks the S3 storage from the schedule matching the target (per-DB or
/// cluster-wide). Falls back to the cluster-wide schedule's storage if no
/// per-DB schedule exists yet, so users can trigger an ad-hoc per-DB backup
/// without pre-configuring a per-DB schedule.
pub async fn trigger_backup(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<TriggerQuery>,
) -> AppResult<impl IntoResponse> {
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
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let catalog_id_str = catalog_id.unwrap_or_default();
    if !crate::backup::executor::supports_backup(&catalog_id_str) {
        return Err(AppError::BadRequest(format!(
            "backup not supported for {catalog_id_str}"
        )));
    }
    if q.database_name.is_some()
        && !crate::backup::executor::supports_per_db_backup(&catalog_id_str)
    {
        return Err(AppError::BadRequest(format!(
            "per-database backup not supported for {catalog_id_str}"
        )));
    }

    // Resolve schedule → s3_storage_id. First try exact match on
    // (service, database_name); fall back to the cluster-wide schedule.
    let (s3_storage_id, schedule_id) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let exact: Option<(String, String)> = db
            .query_row(
                "SELECT s3_storage_id, id FROM backup_schedules
                 WHERE service_id = ?1
                   AND COALESCE(database_name, '') = COALESCE(?2, '')
                   AND is_active = 1",
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
                AppError::BadRequest("No backup schedule configured. Configure one first.".into())
            })?
        }
    };

    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: std::collections::HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();

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

    let backup_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let s3_key = build_s3_key(
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
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, status, size_bytes, error_message, triggered_by, started_at, finished_at,
                s3_key, database_name
         FROM backups WHERE service_id = ?1 ORDER BY started_at DESC LIMIT 50",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
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
            }))
        })?
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
    Path(backup_id): Path<String>,
    Json(body): Json<RestoreRequest>,
) -> AppResult<impl IntoResponse> {
    if body.target_database_name.is_empty() {
        return Err(AppError::BadRequest(
            "target_database_name is required".into(),
        ));
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
        .map_err(|_| AppError::NotFound("Backup not found or not completed".into()))?
    };

    // 2. Service info + env.
    let (name, catalog_id, env_json) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id, env_json FROM services WHERE id = ?1",
            [&service_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Service {service_id} not found")))?
    };

    let catalog_id = catalog_id.unwrap_or_default();
    if !crate::backup::executor::supports_per_db_backup(&catalog_id) {
        return Err(AppError::BadRequest(format!(
            "per-DB restore not supported for {catalog_id}"
        )));
    }

    // 3. Owner credentials for the target DB — must already exist.
    let owner = {
        let creds = load_db_credentials(&state, &service_id)?;
        creds
            .into_iter()
            .find(|c| c.db_name == body.target_database_name)
            .ok_or_else(|| {
                AppError::BadRequest(format!(
                    "target database '{}' does not exist — create it on the Databases tab first",
                    body.target_database_name
                ))
            })?
    };

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
            return Err(AppError::BadRequest(format!(
                "backup contains DB '{db}', refusing to restore into '{}' — pick a backup for the same DB or use a cluster-wide backup",
                body.target_database_name
            )));
        }
    }

    // 6. Decrypt env, dispatch to the catalog-appropriate restore path.
    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: std::collections::HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();
    let container_name = format!("pier-{name}");

    let gzipped = crate::backup::restore::is_gzipped(&s3_key);

    if catalog_id == "mongodb" {
        // Mongo archives are binary BSON — no tar extraction. `--nsInclude`
        // filters the target DB whether the archive is full-instance or per-DB.
        // Gzipped archives are handed to mongorestore with `--gzip` so it
        // decompresses on the fly.
        crate::backup::restore::execute_mongo_restore(
            &container_name,
            &env_vars,
            &body.target_database_name,
            gzipped,
            blob,
        )
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("restore failed: {e}")))?;
    } else {
        // SQL path. Order: gunzip (if needed) → tar-extract (if cluster) →
        // stream into psql/mysql. Legacy `.sql` / `.tar` blobs without the
        // `.gz` suffix bypass the gunzip step so old backups still work.
        let unpacked = if gzipped {
            crate::backup::restore::gunzip_bytes(&blob)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("gunzip: {e}")))?
        } else {
            blob
        };
        let sql_bytes = if crate::backup::restore::is_cluster_archive(&s3_key) {
            crate::backup::restore::extract_db_from_tar(&unpacked, &body.target_database_name)
                .map_err(|e| AppError::BadRequest(e.to_string()))?
        } else {
            unpacked
        };
        crate::backup::restore::execute_restore(
            &container_name,
            &catalog_id,
            &env_vars,
            &body.target_database_name,
            &owner,
            sql_bytes,
        )
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("restore failed: {e}")))?;
    }

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

async fn download_blob(
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
    Path(backup_id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
        .map_err(|_| AppError::NotFound("Backup not found or not completed".into()))?
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
    Path(backup_id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
        .map_err(|_| AppError::NotFound("Backup not found".into()))?
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
