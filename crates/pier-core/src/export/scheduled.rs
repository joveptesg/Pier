//! Scheduled env exports (System → Backups → Env export).
//!
//! Builds the same AES-256 zip as the manual export (`build_archive`) on
//! cron and uploads it to an S3/Bunny storage instead of streaming it to a
//! browser. Each rule lives in `export_schedules` and is mirrored into the
//! unified `schedules` table as `sched-exp-<id>`; the runner fires it
//! through `scheduler::actions::dispatch("export")`.
//!
//! The archive password is stored encrypted with the panel master key —
//! cron has nobody to ask for it.

use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;

use super::{build_archive, load_services, Scope};
use crate::auth::audit::{self, AuthEvent};
use crate::scheduler::cron_utils;
use crate::state::SharedState;

/// Id of the unified-scheduler row mirroring an export rule.
pub fn unified_id(export_schedule_id: &str) -> String {
    format!("sched-exp-{export_schedule_id}")
}

/// UPSERT the `schedules` mirror of an export rule so the runner sees the
/// current cron, enabled flag and next fire time.
pub fn sync_unified_for_export(
    db: &Connection,
    export_schedule_id: &str,
    name: &str,
    cron_expression: &str,
    is_active: bool,
) -> Result<()> {
    let next = cron_utils::next_fire_utc(cron_expression, "UTC", chrono::Utc::now())
        .ok()
        .flatten()
        .map(|t| t.to_rfc3339());
    let config = serde_json::json!({ "export_schedule_id": export_schedule_id }).to_string();
    db.execute(
        "INSERT INTO schedules
            (id, name, cron_expression, timezone, action_type, action_config,
             enabled, is_system, next_run_at)
         VALUES (?1, ?2, ?3, 'UTC', 'export', ?4, ?5, 1, ?6)
         ON CONFLICT(id) DO UPDATE SET
             name            = excluded.name,
             cron_expression = excluded.cron_expression,
             action_config   = excluded.action_config,
             enabled         = excluded.enabled,
             next_run_at     = excluded.next_run_at,
             updated_at      = datetime('now')",
        rusqlite::params![
            unified_id(export_schedule_id),
            format!("Env export: {name}"),
            cron_expression,
            config,
            is_active as i64,
            next,
        ],
    )?;
    Ok(())
}

/// One `s3_storages` row.
pub struct Storage {
    pub storage_type: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub key_prefix: String,
    pub path_style: bool,
}

pub fn load_storage(db: &Connection, id: &str) -> Result<Storage> {
    db.query_row(
        "SELECT storage_type, endpoint, region, bucket, access_key, secret_key, key_prefix,
                path_style
           FROM s3_storages WHERE id = ?1",
        [id],
        |r| {
            Ok(Storage {
                storage_type: r.get(0)?,
                endpoint: r.get(1)?,
                region: r.get(2)?,
                bucket: r.get(3)?,
                access_key: r.get(4)?,
                secret_key: r.get(5)?,
                key_prefix: r.get(6)?,
                path_style: r.get(7)?,
            })
        },
    )
    .with_context(|| format!("storage '{id}' not found"))
}

struct Rule {
    name: String,
    s3_storage_id: String,
    retention: i64,
    project_ids: Option<Vec<String>>,
    include_unassigned: bool,
    include_dumps: bool,
    password_enc: String,
}

/// Run one export rule now: build the archive, upload it, record the run
/// in `export_runs` and apply the rule's retention. `triggered_by` is
/// `"schedule"` for cron fires and `"manual"` for Run now.
pub async fn run_for_schedule(
    state: &SharedState,
    schedule_id: &str,
    triggered_by: &str,
) -> Result<String> {
    let (rule, storage) = {
        let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let rule = db
            .query_row(
                "SELECT name, s3_storage_id, retention_count, project_ids,
                        include_unassigned, include_dumps, archive_password_enc
                   FROM export_schedules WHERE id = ?1",
                [schedule_id],
                |r| {
                    let ids: Option<String> = r.get(3)?;
                    Ok(Rule {
                        name: r.get(0)?,
                        s3_storage_id: r.get(1)?,
                        retention: r.get(2)?,
                        project_ids: ids.and_then(|s| serde_json::from_str(&s).ok()),
                        include_unassigned: r.get(4)?,
                        include_dumps: r.get(5)?,
                        password_enc: r.get(6)?,
                    })
                },
            )
            .map_err(|_| anyhow!("export schedule '{schedule_id}' not found"))?;
        let storage = load_storage(&db, &rule.s3_storage_id)?;
        (rule, storage)
    };

    let run_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().format("%Y%m%d_%H%M%S");
    let file_key = format!("exports/{schedule_id}/pier-export_{ts}.zip");
    let s3_key = if storage.key_prefix.is_empty() {
        file_key
    } else {
        format!("{}/{file_key}", storage.key_prefix)
    };
    {
        let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        db.execute(
            "INSERT INTO export_runs (id, schedule_id, s3_storage_id, s3_key, triggered_by)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                run_id,
                schedule_id,
                rule.s3_storage_id,
                s3_key,
                triggered_by
            ],
        )?;
    }

    match build_and_upload(state, &rule, &storage, &s3_key).await {
        Ok((size, services, dumps)) => {
            {
                let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
                db.execute(
                    "UPDATE export_runs SET status = 'completed', size_bytes = ?1,
                            services_count = ?2, finished_at = datetime('now')
                      WHERE id = ?3",
                    rusqlite::params![size as i64, services as i64, run_id],
                )?;
                db.execute(
                    "UPDATE export_schedules SET last_run_at = datetime('now') WHERE id = ?1",
                    [schedule_id],
                )?;
            }
            audit::log(
                state,
                AuthEvent::SecretsExported,
                None,
                None,
                None,
                Some(serde_json::json!({
                    "scope": "schedule",
                    "schedule_id": schedule_id,
                    "triggered_by": triggered_by,
                    "services": services,
                    "dumps": dumps,
                    "s3_key": s3_key,
                })),
            );
            apply_retention(state, schedule_id, rule.retention).await;
            let msg = format!(
                "Env export '{}' completed: {services} services, {size} bytes → {s3_key}",
                rule.name
            );
            crate::alerts::hooks::fire_event(state, "backup_success", None, msg.clone()).await;
            Ok(msg)
        }
        Err(e) => {
            if let Ok(db) = state.db.lock() {
                let _ = db.execute(
                    "UPDATE export_runs SET status = 'failed', error_message = ?1,
                            finished_at = datetime('now')
                      WHERE id = ?2",
                    rusqlite::params![format!("{e:#}"), run_id],
                );
            }
            crate::alerts::hooks::fire_event(
                state,
                "backup_status",
                None,
                format!("Env export '{}' failed: {e:#}", rule.name),
            )
            .await;
            Err(e)
        }
    }
}

/// Returns (archive size, services, dump files).
async fn build_and_upload(
    state: &SharedState,
    rule: &Rule,
    storage: &Storage,
    s3_key: &str,
) -> Result<(u64, usize, usize)> {
    let password = crate::crypto::decrypt(&rule.password_enc, &crate::crypto::get_secret_key())
        .map_err(|e| anyhow!("decrypt archive password: {e}"))?;
    let scope = Scope::Projects {
        ids: rule.project_ids.clone(),
        include_unassigned: rule.include_unassigned,
    };
    let (_, services) =
        load_services(state, &scope)?.ok_or_else(|| anyhow!("nothing to export"))?;
    let (file, summary) = build_archive(
        state,
        &format!("schedule:{}", rule.name),
        &format!("scheduler:{}", rule.name),
        services,
        password,
        rule.include_dumps,
    )
    .await?;
    let size = file.metadata()?.len();

    match storage.storage_type.as_str() {
        "bunny" => {
            crate::s3::bunny::upload_stream(
                &storage.bucket,
                &storage.access_key,
                &storage.endpoint,
                s3_key,
                tokio::fs::File::from_std(file),
                size,
            )
            .await?;
        }
        _ => {
            let client = crate::s3::build_client(
                &storage.endpoint,
                &storage.region,
                &storage.access_key,
                &storage.secret_key,
                storage.path_style,
            )?;
            crate::s3::upload_stream(
                &client,
                &storage.bucket,
                s3_key,
                tokio::fs::File::from_std(file),
                size,
            )
            .await?;
        }
    }
    Ok((size, summary.services, summary.dumps))
}

/// Keep the newest `retention` completed archives of the rule; delete the
/// rest from storage and from `export_runs`. Failures are logged, not fatal.
async fn apply_retention(state: &SharedState, schedule_id: &str, retention: i64) {
    let old: Vec<(String, String, String)> = {
        let Ok(db) = state.db.lock() else { return };
        let Ok(mut stmt) = db.prepare(
            "SELECT id, s3_storage_id, s3_key FROM export_runs
              WHERE schedule_id = ?1 AND status = 'completed'
              ORDER BY started_at DESC LIMIT -1 OFFSET ?2",
        ) else {
            return;
        };
        stmt.query_map(rusqlite::params![schedule_id, retention.max(1)], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };
    for (id, storage_id, key) in old {
        if let Err(e) =
            crate::backup::scheduler::delete_blob_by_storage_id(state, &storage_id, &key).await
        {
            tracing::warn!("export retention: failed to delete {key} from {storage_id}: {e}");
            continue;
        }
        if let Ok(db) = state.db.lock() {
            let _ = db.execute("DELETE FROM export_runs WHERE id = ?1", [&id]);
        }
    }
}
