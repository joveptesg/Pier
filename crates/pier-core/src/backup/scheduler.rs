use std::collections::HashMap;

use tokio::time::{interval, Duration};

use crate::state::SharedState;

/// Start the background backup scheduler.
/// Checks every 60 seconds for schedules whose next_run_at <= now.
pub fn start_scheduler(state: SharedState) {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            if let Err(e) = check_and_run(&state).await {
                tracing::error!("Backup scheduler error: {e}");
            }
        }
    });
}

async fn check_and_run(state: &SharedState) -> anyhow::Result<()> {
    let due_schedules: Vec<(String, String, String, String, i64)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT bs.id, bs.service_id, bs.s3_storage_id, bs.cron_expression, bs.retention_count
             FROM backup_schedules bs
             WHERE bs.is_active = 1 AND bs.next_run_at <= datetime('now')",
        )?;
        let result: Vec<(String, String, String, String, i64)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        result
    };

    for (schedule_id, service_id, s3_id, _cron_expr, retention) in due_schedules {
        tracing::info!("Running backup for schedule {schedule_id}");
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) =
                run_single_backup(&state, &schedule_id, &service_id, &s3_id, retention).await
            {
                tracing::error!("Backup failed for schedule {schedule_id}: {e}");
            }
        });
    }

    Ok(())
}

async fn run_single_backup(
    state: &SharedState,
    schedule_id: &str,
    service_id: &str,
    s3_storage_id: &str,
    retention: i64,
) -> anyhow::Result<()> {
    // 1. Get resource info
    let (name, catalog_id, env_json) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id, env_json FROM services WHERE id = ?1",
            [service_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )?
    };

    let catalog_id = catalog_id.unwrap_or_default();
    let env_vars: HashMap<String, String> = env_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default();

    // 2. Get S3 storage config
    let (storage_type, endpoint, region, bucket, access_key, secret_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key FROM s3_storages WHERE id = ?1",
            [s3_storage_id],
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

    // 3. Create backup record
    let backup_id = uuid::Uuid::new_v4().to_string();
    let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
    let s3_key = format!("pier-backups/{name}/{catalog_id}_{timestamp}.dump");

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO backups (id, schedule_id, service_id, s3_storage_id, s3_key, status, triggered_by)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', 'schedule')",
            rusqlite::params![backup_id, schedule_id, service_id, s3_storage_id, s3_key],
        )?;
    }

    // 4. Execute docker exec dump
    let container_name = format!("pier-{name}");
    let data = match super::executor::execute_backup(&container_name, &catalog_id, &env_vars).await
    {
        Ok(d) => d,
        Err(e) => {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            db.execute(
                "UPDATE backups SET status = 'failed', error_message = ?1, finished_at = datetime('now') WHERE id = ?2",
                rusqlite::params![e.to_string(), backup_id],
            )?;
            return Err(e);
        }
    };

    let size = data.len() as i64;

    // 5. Upload to S3 / Bunny.net
    let upload_result = match storage_type.as_str() {
        "bunny" => {
            crate::s3::bunny::upload_file(&bucket, &access_key, &endpoint, &s3_key, data).await
        }
        _ => {
            let client = crate::s3::build_client(&endpoint, &region, &access_key, &secret_key)?;
            crate::s3::upload_file(&client, &bucket, &s3_key, data).await
        }
    };

    if let Err(e) = upload_result {
        let db = state
            .db
            .lock()
            .map_err(|er| anyhow::anyhow!("DB lock: {er}"))?;
        db.execute(
            "UPDATE backups SET status = 'failed', error_message = ?1, finished_at = datetime('now') WHERE id = ?2",
            rusqlite::params![e.to_string(), backup_id],
        )?;
        return Err(e);
    }

    // 6. Mark backup as completed
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE backups SET status = 'completed', size_bytes = ?1, finished_at = datetime('now') WHERE id = ?2",
            rusqlite::params![size, backup_id],
        )?;

        // 7. Update schedule
        db.execute(
            "UPDATE backup_schedules SET last_run_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
            [schedule_id],
        )?;

        // Compute next_run_at (add 24h as simple approximation; proper cron parsing would use cron crate)
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+1 day') WHERE id = ?1 AND cron_expression = '0 2 * * *'",
            [schedule_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+7 days') WHERE id = ?1 AND cron_expression = '0 2 * * 0'",
            [schedule_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+6 hours') WHERE id = ?1 AND cron_expression = '0 */6 * * *'",
            [schedule_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+1 hour') WHERE id = ?1 AND cron_expression = '0 * * * *'",
            [schedule_id],
        )?;

        // 8. Apply retention policy
        let old_backups: Vec<String> = {
            let mut stmt = db.prepare(
                "SELECT id FROM backups WHERE schedule_id = ?1 AND status = 'completed'
                 ORDER BY started_at DESC LIMIT -1 OFFSET ?2",
            )?;
            let result: Vec<String> = stmt
                .query_map(rusqlite::params![schedule_id, retention], |row| {
                    row.get::<_, String>(0)
                })?
                .filter_map(|r| r.ok())
                .collect();
            result
        };

        for old_id in old_backups {
            db.execute("DELETE FROM backups WHERE id = ?1", [&old_id])?;
        }
    }

    tracing::info!("Backup {backup_id} completed: {size} bytes uploaded to {s3_key}");
    Ok(())
}
