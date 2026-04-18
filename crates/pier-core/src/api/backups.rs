use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateScheduleRequest {
    pub s3_storage_id: String,
    #[serde(default = "default_cron")]
    pub cron_expression: String,
    #[serde(default = "default_retention")]
    pub retention_count: i64,
}

fn default_cron() -> String {
    "0 2 * * *".to_string()
}
fn default_retention() -> i64 {
    7
}

/// POST /api/v1/resources/{id}/backups/schedule
pub async fn create_schedule(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<CreateScheduleRequest>,
) -> AppResult<impl IntoResponse> {
    if body.s3_storage_id.is_empty() {
        return Err(AppError::BadRequest("S3 storage is required".into()));
    }

    let schedule_id = uuid::Uuid::new_v4().to_string();

    // Compute initial next_run_at based on cron expression
    let next_run = match body.cron_expression.as_str() {
        "0 2 * * *" => "+1 day",
        "0 2 * * 0" => "+7 days",
        "0 */6 * * *" => "+6 hours",
        "0 * * * *" => "+1 hour",
        _ => "+1 day",
    };

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Delete existing schedule for this service
    db.execute("DELETE FROM backup_schedules WHERE service_id = ?1", [&id])?;

    db.execute(
        "INSERT INTO backup_schedules (id, service_id, s3_storage_id, cron_expression, retention_count, next_run_at)
         VALUES (?1, ?2, ?3, ?4, ?5, datetime('now', ?6))",
        rusqlite::params![
            schedule_id,
            id,
            body.s3_storage_id,
            body.cron_expression,
            body.retention_count,
            next_run
        ],
    )?;

    Ok(Json(serde_json::json!({"ok": true, "id": schedule_id})))
}

/// GET /api/v1/resources/{id}/backups/schedule
pub async fn get_schedule(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let result = db.query_row(
        "SELECT id, s3_storage_id, cron_expression, retention_count, is_active, last_run_at, next_run_at
         FROM backup_schedules WHERE service_id = ?1 AND is_active = 1",
        [&id],
        |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "s3_storage_id": row.get::<_, String>(1)?,
                "cron_expression": row.get::<_, String>(2)?,
                "retention_count": row.get::<_, i64>(3)?,
                "is_active": row.get::<_, bool>(4)?,
                "last_run_at": row.get::<_, Option<String>>(5)?,
                "next_run_at": row.get::<_, Option<String>>(6)?,
            }))
        },
    );

    match result {
        Ok(schedule) => Ok(Json(schedule)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(Json(serde_json::Value::Null)),
        Err(e) => Err(AppError::Database(e)),
    }
}

/// DELETE /api/v1/resources/{id}/backups/schedule/{schedule_id}
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

/// POST /api/v1/resources/{id}/backups/trigger
pub async fn trigger_backup(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    // Get resource info
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

    // Find the schedule's S3 storage (or return error)
    let (s3_storage_id, schedule_id) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT s3_storage_id, id FROM backup_schedules WHERE service_id = ?1 AND is_active = 1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::BadRequest("No backup schedule configured. Configure one first.".into())
        })?
    };

    let catalog_id_str = catalog_id.unwrap_or_default();
    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: std::collections::HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();

    // Get S3 config
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

    // Create backup record
    let backup_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let s3_key = format!("pier-backups/{name}/{catalog_id_str}_{timestamp}.dump");

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO backups (id, schedule_id, service_id, s3_storage_id, s3_key, status, triggered_by)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', 'manual')",
            rusqlite::params![backup_id, schedule_id, id, s3_storage_id, s3_key],
        )?;
    }

    // Spawn backup in background
    let bid = backup_id.clone();
    let state2 = state.clone();
    tokio::spawn(async move {
        let container_name = format!("pier-{name}");
        let data = match crate::backup::executor::execute_backup(
            &container_name,
            &catalog_id_str,
            &env_vars,
        )
        .await
        {
            Ok(d) => d,
            Err(e) => {
                if let Ok(db) = state2.db.lock() {
                    let _ = db.execute(
                        "UPDATE backups SET status='failed', error_message=?1, finished_at=datetime('now') WHERE id=?2",
                        rusqlite::params![e.to_string(), bid],
                    );
                }
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
        "SELECT id, status, size_bytes, error_message, triggered_by, started_at, finished_at, s3_key
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
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
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

    // For S3, generate a presigned URL or proxy the download
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
