//! Core ↔ pier-agent shell-task driver.
//!
//! Flow:
//!
//! 1. [`start_run`] inserts a pending row, POSTs `/api/v1/agent/shell`
//!    on the target agent, stashes the returned `run_id` in
//!    `task_runs.agent_run_id`, flips status to `running` and spawns
//!    [`drive_run`].
//!
//! 2. [`drive_run`] polls `GET /shell/{run_id}` every `POLL_INTERVAL` and
//!    mirrors the agent's snapshot into the DB. Stops when the agent
//!    reports a terminal status or after `MAX_POLL_FAILURES` consecutive
//!    transport errors.
//!
//! 3. [`cancel_run`] flips the DB row to `cancelled` and POSTs the agent's
//!    `/shell/{id}/cancel`. The poller picks up the new status on its
//!    next iteration and stops.

use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::Deserialize;

use crate::api::servers::get_server_info;
use crate::error::AppError;
use crate::state::SharedState;
use crate::tasks::models;

/// Cadence at which we poll the agent for a running task. ~1 s is fast
/// enough for a UI tail without flooding the agent.
const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Consecutive transport errors before we declare the run `unreachable`.
/// 6 = ~6 s of agent silence.
const MAX_POLL_FAILURES: u32 = 6;

#[derive(Deserialize, Debug, Clone)]
struct AgentSnapshot {
    run_id: String,
    status: String,
    exit_code: Option<i64>,
    stdout: String,
    stderr: String,
    finished_at_unix_ms: Option<i64>,
    error: Option<String>,
}

#[derive(Deserialize, Debug)]
struct AgentStart {
    run_id: String,
}

/// Spec passed by the API handler.
pub struct StartSpec {
    pub server_id: String,
    pub template_id: Option<String>,
    pub command: String,
    pub env: serde_json::Map<String, serde_json::Value>,
    pub timeout_sec: i64,
    pub triggered_by: String,
}

/// Insert a row, start the agent run, and spawn the poller. Returns the
/// new `task_runs.id`.
pub async fn start_run(state: &SharedState, spec: StartSpec) -> Result<String, AppError> {
    let env_json = serde_json::to_string(&spec.env).unwrap_or_else(|_| "{}".to_string());

    let task_id = {
        let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        models::run_insert_pending(
            &db,
            &spec.server_id,
            spec.template_id.as_deref(),
            None,
            &spec.command,
            &env_json,
            spec.timeout_sec,
            &spec.triggered_by,
        )
        .map_err(|e| anyhow!("insert task_run: {e}"))?
    };

    let (host, port, agent_token, is_local, kind) = get_server_info(state, &spec.server_id)?;
    if kind == "peer" {
        return finalize_unreachable(state, &task_id, "tasks not supported on peer servers");
    }
    if is_local || host.is_empty() {
        return finalize_unreachable(
            state,
            &task_id,
            "tasks on the local server require a running pier-agent",
        );
    }

    let agent_run_id = match call_agent_start(
        &host,
        port,
        &agent_token,
        &spec.command,
        spec.timeout_sec,
        &spec.env,
    )
    .await
    {
        Ok(r) => r.run_id,
        Err(e) => {
            return finalize_unreachable(state, &task_id, &format!("agent unreachable: {e}"));
        }
    };

    {
        let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        db.execute(
            "UPDATE task_runs SET agent_run_id = ?1, status = 'running' WHERE id = ?2",
            rusqlite::params![agent_run_id, task_id],
        )
        .map_err(|e| anyhow!("update task_run: {e}"))?;
    }

    spawn_driver(
        state.clone(),
        task_id.clone(),
        spec.server_id.clone(),
        agent_run_id,
    );
    Ok(task_id)
}

/// Background poller — pulls snapshots from the agent until terminal.
pub fn spawn_driver(state: SharedState, task_id: String, server_id: String, agent_run_id: String) {
    tokio::spawn(async move {
        let mut failures: u32 = 0;
        loop {
            tokio::time::sleep(POLL_INTERVAL).await;
            // Re-read server info each iteration so token rotation /
            // mesh transitions don't strand the poller against a stale
            // host/port.
            let conn_info = get_server_info(&state, &server_id);
            let (host, port, agent_token) = match conn_info {
                Ok((h, p, t, _, _)) => (h, p, t),
                Err(e) => {
                    failures += 1;
                    if failures >= MAX_POLL_FAILURES {
                        let _ = finalize_unreachable(
                            &state,
                            &task_id,
                            &format!("server lookup failed: {e}"),
                        );
                        return;
                    }
                    continue;
                }
            };

            match fetch_agent_snapshot(&host, port, &agent_token, &agent_run_id).await {
                Ok(snap) => {
                    failures = 0;
                    let finished =
                        models::is_terminal(&snap.status) || snap.finished_at_unix_ms.is_some();
                    {
                        let db = match state.db.lock() {
                            Ok(g) => g,
                            Err(_) => continue,
                        };
                        let _ = models::run_update_snapshot(
                            &db,
                            &task_id,
                            Some(&snap.run_id),
                            &snap.status,
                            snap.exit_code,
                            &snap.stdout,
                            &snap.stderr,
                            finished,
                            snap.error.as_deref(),
                        );
                    }
                    if finished {
                        return;
                    }
                }
                Err(e) => {
                    failures += 1;
                    tracing::debug!(
                        task_id, %e, "task poll attempt {failures}/{MAX_POLL_FAILURES} failed"
                    );
                    if failures >= MAX_POLL_FAILURES {
                        let _ = finalize_unreachable(
                            &state,
                            &task_id,
                            &format!("agent unreachable: {e}"),
                        );
                        return;
                    }
                }
            }
        }
    });
}

/// User-initiated cancel. Flips the DB row first (so the UI can react
/// immediately) and tells the agent to SIGTERM the child. The driver
/// picks up the agent's terminal snapshot on its next poll and finalises.
pub async fn cancel_run(state: &SharedState, task_id: &str) -> Result<(), AppError> {
    let (server_id, agent_run_id) = {
        let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
        let run = models::run_get(&db, task_id)
            .map_err(|e| anyhow!("read task_run: {e}"))?
            .ok_or_else(|| {
                AppError::NotFound(crate::i18n::te("errors.executor.task_run_not_found"))
            })?;
        if models::is_terminal(&run.status) {
            return Err(AppError::Conflict(crate::i18n::te_args(
                "errors.executor.task_already_status",
                &[("v", &run.status)],
            )));
        }
        let _ = models::run_mark_cancelled(&db, task_id);
        (run.server_id, run.agent_run_id)
    };

    if let Some(agent_run_id) = agent_run_id {
        let (host, port, agent_token, _, _) = get_server_info(state, &server_id)?;
        let _ = call_agent_cancel(&host, port, &agent_token, &agent_run_id).await;
    }
    Ok(())
}

fn finalize_unreachable(state: &SharedState, task_id: &str, msg: &str) -> Result<String, AppError> {
    let db = state.db.lock().map_err(|e| anyhow!("DB lock: {e}"))?;
    let _ = models::run_mark_unreachable(&db, task_id, msg);
    Ok(task_id.to_string())
}

async fn call_agent_start(
    host: &str,
    port: i64,
    token: &str,
    command: &str,
    timeout_sec: i64,
    env: &serde_json::Map<String, serde_json::Value>,
) -> Result<AgentStart> {
    let url = format!(
        "http://{}/api/v1/agent/shell",
        crate::network::address::authority(host, port)
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "command": command,
            "timeout_sec": timeout_sec,
            "env": env,
        }))
        .send()
        .await
        .map_err(|e| anyhow!("post: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("agent returned {status}: {body}"));
    }
    let parsed: AgentStart = resp
        .json()
        .await
        .map_err(|e| anyhow!("decode start response: {e}"))?;
    Ok(parsed)
}

async fn fetch_agent_snapshot(
    host: &str,
    port: i64,
    token: &str,
    agent_run_id: &str,
) -> Result<AgentSnapshot> {
    let url = format!("http://{host}:{port}/api/v1/agent/shell/{agent_run_id}");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| anyhow!("get: {e}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Err(anyhow!("agent lost track of the run (likely restarted)"));
    }
    if !resp.status().is_success() {
        return Err(anyhow!("agent returned {}", resp.status()));
    }
    let snap: AgentSnapshot = resp
        .json()
        .await
        .map_err(|e| anyhow!("decode snapshot: {e}"))?;
    Ok(snap)
}

async fn call_agent_cancel(host: &str, port: i64, token: &str, agent_run_id: &str) -> Result<()> {
    let url = format!("http://{host}:{port}/api/v1/agent/shell/{agent_run_id}/cancel");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| anyhow!("http client: {e}"))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| anyhow!("post: {e}"))?;
    if !resp.status().is_success() {
        return Err(anyhow!("agent returned {}", resp.status()));
    }
    Ok(())
}
