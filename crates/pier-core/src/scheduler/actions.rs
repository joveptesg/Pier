//! Per-action dispatchers for the unified scheduler.
//!
//! The runner deserialises `schedules.action_config` and calls one of
//! these. Each returns the result string that ends up in
//! `schedule_runs.output`, plus an optional `task_run_id` link.

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::backup;
use crate::docker::cleanup::{self, CleanupOptions};
use crate::state::SharedState;
use crate::tasks::{executor, models};

/// One action fire result.
pub struct ActionResult {
    pub output: String,
    pub task_run_id: Option<String>,
}

#[derive(Deserialize)]
struct TaskActionConfig {
    template_id: String,
    server_id: String,
}

#[derive(Deserialize)]
struct BackupActionConfig {
    backup_schedule_id: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CleanupActionConfig {
    prune_images: Option<bool>,
    prune_build_cache: Option<bool>,
    prune_containers: Option<bool>,
}

/// Dispatch a single schedule fire based on its `action_type`.
///
/// Returns `Ok(ActionResult)` on a successful fire (or when the action's
/// own handler reports a tolerable outcome). `Err` is reserved for hard
/// failures (bad config, unreachable handler, etc).
pub async fn dispatch(
    state: &SharedState,
    _schedule_id: &str,
    action_type: &str,
    action_config: &str,
    triggered_by: &str,
) -> Result<ActionResult> {
    match action_type {
        "task" => fire_task(state, action_config, triggered_by).await,
        "backup" => fire_backup(state, action_config).await,
        "cleanup" => fire_cleanup(state, action_config).await,
        other => Err(anyhow!("unknown action_type '{other}'")),
    }
}

async fn fire_task(
    state: &SharedState,
    action_config: &str,
    triggered_by: &str,
) -> Result<ActionResult> {
    let cfg: TaskActionConfig = serde_json::from_str(action_config)
        .map_err(|e| anyhow!("invalid task action_config: {e}"))?;

    let tmpl = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow!("DB lock: {e}"))?;
        models::template_get(&db, &cfg.template_id)?
            .ok_or_else(|| anyhow!("task template '{}' not found", cfg.template_id))?
    };
    let env: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&tmpl.default_env_json).unwrap_or_default();

    let task_id = executor::start_run(
        state,
        executor::StartSpec {
            server_id: cfg.server_id,
            template_id: Some(cfg.template_id),
            command: tmpl.command,
            env,
            timeout_sec: tmpl.default_timeout_sec,
            triggered_by: triggered_by.to_string(),
        },
    )
    .await
    .map_err(|e| anyhow!("start task run: {e:?}"))?;

    Ok(ActionResult {
        output: format!("started task_run {task_id}"),
        task_run_id: Some(task_id),
    })
}

async fn fire_backup(state: &SharedState, action_config: &str) -> Result<ActionResult> {
    let cfg: BackupActionConfig = serde_json::from_str(action_config)
        .map_err(|e| anyhow!("invalid backup action_config: {e}"))?;
    let output = backup::scheduler::run_for_schedule(state, &cfg.backup_schedule_id).await?;
    Ok(ActionResult {
        output,
        task_run_id: None,
    })
}

async fn fire_cleanup(state: &SharedState, action_config: &str) -> Result<ActionResult> {
    let cfg: CleanupActionConfig =
        serde_json::from_str(action_config).unwrap_or_default();
    let defaults = CleanupOptions::defaults();
    let opts = CleanupOptions {
        prune_images: cfg.prune_images.unwrap_or(defaults.prune_images),
        prune_build_cache: cfg.prune_build_cache.unwrap_or(defaults.prune_build_cache),
        prune_containers: cfg.prune_containers.unwrap_or(defaults.prune_containers),
    };
    let output = cleanup::run_once(state, &opts).await?;
    Ok(ActionResult {
        output,
        task_run_id: None,
    })
}
