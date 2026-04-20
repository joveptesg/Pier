use std::collections::HashMap;

use tokio::time::{interval, Duration};

use crate::state::SharedState;

use super::executor::DbCredential;

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

struct DueSchedule {
    schedule_id: String,
    service_id: String,
    s3_storage_id: String,
    retention: i64,
    database_name: Option<String>,
}

async fn check_and_run(state: &SharedState) -> anyhow::Result<()> {
    let due_schedules: Vec<DueSchedule> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT bs.id, bs.service_id, bs.s3_storage_id, bs.retention_count, bs.database_name
             FROM backup_schedules bs
             WHERE bs.is_active = 1 AND bs.next_run_at <= datetime('now')",
        )?;
        let result: Vec<DueSchedule> = stmt
            .query_map([], |row| {
                Ok(DueSchedule {
                    schedule_id: row.get::<_, String>(0)?,
                    service_id: row.get::<_, String>(1)?,
                    s3_storage_id: row.get::<_, String>(2)?,
                    retention: row.get::<_, i64>(3)?,
                    database_name: row.get::<_, Option<String>>(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        result
    };

    for due in due_schedules {
        tracing::info!(
            "Running backup for schedule {} (db={:?})",
            due.schedule_id,
            due.database_name
        );
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = run_single_backup(&state, &due).await {
                tracing::error!("Backup failed for schedule {}: {e}", due.schedule_id);
            }
        });
    }

    Ok(())
}

/// Fetch every database credential row for a service. Used to drive
/// cluster-wide backups (loop over every DB) and to resolve the owner/password
/// of a single DB during per-DB backups.
pub fn load_db_credentials(
    state: &SharedState,
    service_id: &str,
) -> anyhow::Result<Vec<DbCredential>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT db_name, username, password FROM database_credentials WHERE service_id = ?1
         ORDER BY db_name",
    )?;
    let rows: Vec<DbCredential> = stmt
        .query_map([service_id], |row| {
            Ok(DbCredential {
                db_name: row.get::<_, String>(0)?,
                username: row.get::<_, String>(1)?,
                password: row.get::<_, String>(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

async fn run_single_backup(state: &SharedState, due: &DueSchedule) -> anyhow::Result<()> {
    // 1. Get resource info
    let (name, catalog_id, env_json) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT name, catalog_id, env_json FROM services WHERE id = ?1",
            [&due.service_id],
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
    let decrypted_env = crate::crypto::decrypt_env_json(env_json.as_deref());
    let env_vars: HashMap<String, String> =
        serde_json::from_str(&decrypted_env).unwrap_or_default();

    // 2. Get S3 storage config
    let (storage_type, endpoint, region, bucket, access_key, secret_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key FROM s3_storages WHERE id = ?1",
            [&due.s3_storage_id],
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
    let s3_key = build_s3_key(&name, &catalog_id, due.database_name.as_deref(), &timestamp);

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO backups (id, schedule_id, service_id, s3_storage_id, s3_key, database_name, status, triggered_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', 'schedule')",
            rusqlite::params![backup_id, due.schedule_id, due.service_id, due.s3_storage_id, s3_key, due.database_name],
        )?;
    }

    // 4. Execute dump (per-DB or cluster-wide)
    let container_name = format!("pier-{name}");
    let dump_result = match &due.database_name {
        Some(db_name) => {
            let creds = load_db_credentials(state, &due.service_id)?;
            let cred = creds.iter().find(|c| &c.db_name == db_name);
            super::executor::execute_db_backup(
                &container_name,
                &catalog_id,
                &env_vars,
                cred,
                db_name,
            )
            .await
        }
        None => {
            let creds = load_db_credentials(state, &due.service_id)?;
            super::executor::execute_cluster_backup(&container_name, &catalog_id, &env_vars, &creds)
                .await
        }
    };

    let data = match dump_result {
        Ok(d) => d,
        Err(e) => {
            {
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "UPDATE backups SET status = 'failed', error_message = ?1, finished_at = datetime('now') WHERE id = ?2",
                    rusqlite::params![e.to_string(), backup_id],
                )?;
            }
            crate::alerts::hooks::fire_event(
                state,
                "backup_status",
                Some(&due.service_id),
                format!("Backup dump failed for {name}: {e}"),
            )
            .await;
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
        {
            let db = state
                .db
                .lock()
                .map_err(|er| anyhow::anyhow!("DB lock: {er}"))?;
            db.execute(
                "UPDATE backups SET status = 'failed', error_message = ?1, finished_at = datetime('now') WHERE id = ?2",
                rusqlite::params![e.to_string(), backup_id],
            )?;
        }
        crate::alerts::hooks::fire_event(
            state,
            "backup_status",
            Some(&due.service_id),
            format!("Backup upload failed for {name}: {e}"),
        )
        .await;
        return Err(e);
    }

    // 6. Mark backup as completed, advance schedule, collect retention list.
    // The DB lock is a std::sync::Mutex, so we can't hold it across awaits —
    // all DB work happens here synchronously and the block returns the list
    // of old backups to delete async.
    let old_backups: Vec<(String, String, String)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE backups SET status = 'completed', size_bytes = ?1, finished_at = datetime('now') WHERE id = ?2",
            rusqlite::params![size, backup_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET last_run_at = datetime('now'), updated_at = datetime('now') WHERE id = ?1",
            [&due.schedule_id],
        )?;
        // Crude next_run_at advancement (existing behavior; proper cron
        // parsing is tech debt tracked separately).
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+1 day') WHERE id = ?1 AND cron_expression = '0 2 * * *'",
            [&due.schedule_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+7 days') WHERE id = ?1 AND cron_expression = '0 2 * * 0'",
            [&due.schedule_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+6 hours') WHERE id = ?1 AND cron_expression = '0 */6 * * *'",
            [&due.schedule_id],
        )?;
        db.execute(
            "UPDATE backup_schedules SET next_run_at = datetime('now', '+1 hour') WHERE id = ?1 AND cron_expression = '0 * * * *'",
            [&due.schedule_id],
        )?;
        let mut stmt = db.prepare(
            "SELECT id, s3_storage_id, s3_key FROM backups
             WHERE schedule_id = ?1 AND status = 'completed'
             ORDER BY started_at DESC LIMIT -1 OFFSET ?2",
        )?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map(rusqlite::params![due.schedule_id, due.retention], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    for (old_id, storage_id, key) in old_backups {
        if let Err(e) = delete_blob_by_storage_id(state, &storage_id, &key).await {
            // Don't fail the backup run — the new backup succeeded, and a
            // stuck cleanup shouldn't abort that. Log and carry on.
            tracing::warn!(
                "retention: failed to delete old blob {key} from storage {storage_id}: {e}"
            );
        }
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute("DELETE FROM backups WHERE id = ?1", [&old_id])?;
    }

    tracing::info!("Backup {backup_id} completed: {size} bytes uploaded to {s3_key}");

    crate::alerts::hooks::fire_event(
        state,
        "backup_success",
        Some(&due.service_id),
        format!("Backup succeeded for {name} ({size} bytes → {s3_key})"),
    )
    .await;

    Ok(())
}

/// Look up an S3 storage row and delete the given blob from whichever
/// backend (S3 or Bunny) it points to. Used by retention cleanup so old
/// backups don't leave orphaned blobs behind.
async fn delete_blob_by_storage_id(
    state: &SharedState,
    s3_storage_id: &str,
    s3_key: &str,
) -> anyhow::Result<()> {
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
    crate::s3::delete_blob(
        &storage_type,
        &endpoint,
        &region,
        &bucket,
        &access_key,
        &secret_key,
        s3_key,
    )
    .await
}

/// S3 key format (all backups are compressed; `.gz` suffix indicates the
/// blob needs decompression before use — except for Mongo archives, which
/// are consumed by mongorestore with its own `--gzip` flag):
///   - per-DB mongo:  pier-backups/{service}/db_{dbname}_{timestamp}.archive.gz
///   - per-DB SQL:    pier-backups/{service}/db_{dbname}_{timestamp}.sql.gz
///   - cluster tar:   pier-backups/{service}/_cluster_{timestamp}.tar.gz
///   - mongo full:    pier-backups/{service}/mongodb_{timestamp}.archive.gz
pub fn build_s3_key(
    service_name: &str,
    catalog_id: &str,
    database_name: Option<&str>,
    timestamp: &str,
) -> String {
    match database_name {
        Some(db) if catalog_id == "mongodb" => {
            format!("pier-backups/{service_name}/db_{db}_{timestamp}.archive.gz")
        }
        Some(db) => format!("pier-backups/{service_name}/db_{db}_{timestamp}.sql.gz"),
        None if catalog_id == "mongodb" => {
            format!("pier-backups/{service_name}/mongodb_{timestamp}.archive.gz")
        }
        None => format!("pier-backups/{service_name}/_cluster_{timestamp}.tar.gz"),
    }
}
