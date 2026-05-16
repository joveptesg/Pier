//! Ad-hoc shell-runner endpoints.
//!
//! Protocol (all under `/api/v1/agent/shell/**`):
//!
//! * `POST /shell` — start a run. Body `{command, timeout_sec?, env?}`,
//!   response `{run_id}` (hex string). The agent forks `bash -c …` with
//!   piped stdout / stderr and a per-run [`RunHandle`] is registered in
//!   `AgentState.shell_runs`.
//!
//! * `GET /shell/{run_id}` — snapshot poll. Returns
//!   `{status, exit_code?, stdout, stderr, started_at, finished_at?}`.
//!   Core polls this every ~500 ms while a run is `running` to drive the
//!   Tasks UI.
//!
//! * `POST /shell/{run_id}/cancel` — sends SIGTERM via `kill(pid)`. Status
//!   transitions to `cancelled`. The reader tasks unwind once the pipes
//!   close.
//!
//! Caps: stdout / stderr are bounded at [`MAX_STREAM_BYTES`] each; anything
//! past that is dropped with a one-line truncation marker so the agent
//! never grows unbounded memory on a runaway producer.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

use crate::{require_auth, SharedState};

/// Max stdout / stderr bytes retained per stream. 5 MiB matches the
/// deployment-log cap on core; beyond that we truncate with a marker.
const MAX_STREAM_BYTES: usize = 5 * 1024 * 1024;

/// In-memory state for a single shell run. Held inside
/// `AgentState.shell_runs` keyed by `run_id`.
#[allow(dead_code)] // `command` / `started_instant` retained for debug / future telemetry.
pub struct RunHandle {
    pub run_id: String,
    pub command: String,
    pub timeout_sec: u32,
    pub started_at_unix_ms: i64,
    pub started_instant: Instant,
    /// OS pid of the spawned `bash -c …`. Used to send SIGTERM on cancel
    /// without holding the `Child` across awaits.
    pub pid: Option<u32>,
    /// `running` | `success` | `failed` | `cancelled` | `timeout`.
    pub status: RwLock<String>,
    pub exit_code: RwLock<Option<i32>>,
    pub finished_at_unix_ms: RwLock<Option<i64>>,
    pub error: RwLock<Option<String>>,
    pub stdout: Mutex<StreamBuf>,
    pub stderr: Mutex<StreamBuf>,
}

#[derive(Default)]
pub struct StreamBuf {
    buf: Vec<u8>,
    truncated: bool,
}

impl StreamBuf {
    fn append_line(&mut self, line: &str) {
        if self.truncated {
            return;
        }
        let remaining = MAX_STREAM_BYTES.saturating_sub(self.buf.len());
        if remaining == 0 {
            self.truncated = true;
            self.buf
                .extend_from_slice(b"\n[output truncated -- exceeded 5 MiB cap]\n");
            return;
        }
        if line.len() + 1 > remaining {
            // Partial copy then stop; better than dropping the tail
            // entirely on a long single line.
            self.buf
                .extend_from_slice(&line.as_bytes()[..remaining.min(line.len())]);
            self.buf
                .extend_from_slice(b"\n[output truncated -- exceeded 5 MiB cap]\n");
            self.truncated = true;
            return;
        }
        self.buf.extend_from_slice(line.as_bytes());
        self.buf.push(b'\n');
    }

    fn snapshot_string(&self) -> String {
        String::from_utf8_lossy(&self.buf).into_owned()
    }
}

#[derive(Deserialize)]
pub struct StartRequest {
    pub command: String,
    #[serde(default)]
    pub timeout_sec: Option<u32>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

#[derive(Serialize)]
pub struct StartResponse {
    pub run_id: String,
}

#[derive(Serialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub status: String,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub started_at_unix_ms: i64,
    pub finished_at_unix_ms: Option<i64>,
    pub error: Option<String>,
}

/// Validate an env var key. Conservative POSIX-style alphabet so we never
/// feed exotic bytes to `bash -c`.
fn is_safe_env_key(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let bytes = key.as_bytes();
    let first = bytes[0];
    if !(first.is_ascii_uppercase() || first == b'_') {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub async fn start_run(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Json(body): Json<StartRequest>,
) -> axum::response::Response {
    require_auth!(headers, state);

    let command = body.command.trim().to_string();
    if command.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "command cannot be empty"})),
        )
            .into_response();
    }
    let timeout_sec = body.timeout_sec.unwrap_or(1800).clamp(1, 24 * 3600);

    let rid_bytes: [u8; 16] = rand::random();
    let run_id = hex::encode(rid_bytes);

    // Build env: caller-supplied (filtered) + pier metadata.
    let mut child_env: Vec<(String, String)> = Vec::new();
    for (k, v) in body.env.iter() {
        if is_safe_env_key(k) {
            child_env.push((k.clone(), v.clone()));
        }
    }
    child_env.push(("PIER_TASK_RUN_ID".into(), run_id.clone()));

    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(&command);
    for (k, v) in &child_env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("spawn failed: {e}"),
                })),
            )
                .into_response();
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let pid = child.id();

    let handle = Arc::new(RunHandle {
        run_id: run_id.clone(),
        command,
        timeout_sec,
        started_at_unix_ms: now_unix_ms(),
        started_instant: Instant::now(),
        pid,
        status: RwLock::new("running".into()),
        exit_code: RwLock::new(None),
        finished_at_unix_ms: RwLock::new(None),
        error: RwLock::new(None),
        stdout: Mutex::new(StreamBuf::default()),
        stderr: Mutex::new(StreamBuf::default()),
    });

    state
        .shell_runs
        .write()
        .await
        .insert(run_id.clone(), handle.clone());

    // Reader for stdout.
    if let Some(out) = stdout {
        let h = handle.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(out).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                h.stdout.lock().await.append_line(&line);
            }
        });
    }
    // Reader for stderr.
    if let Some(err) = stderr {
        let h = handle.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(err).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                h.stderr.lock().await.append_line(&line);
            }
        });
    }

    // Supervisor: races wait() vs timeout, records terminal status.
    let h = handle.clone();
    tokio::spawn(async move {
        let timeout = Duration::from_secs(h.timeout_sec as u64);
        let wait_fut = child.wait();
        tokio::pin!(wait_fut);

        let result = tokio::select! {
            r = &mut wait_fut => Some(r),
            _ = sleep(timeout) => None,
        };

        match result {
            None => {
                // Timeout: kill the process and finalise.
                if let Some(pid) = h.pid {
                    send_sigterm(pid).await;
                }
                let _ = wait_fut.await;
                let mut s = h.status.write().await;
                if *s == "running" {
                    *s = "timeout".into();
                }
                drop(s);
                *h.finished_at_unix_ms.write().await = Some(now_unix_ms());
                *h.error.write().await =
                    Some(format!("exceeded timeout of {}s", h.timeout_sec));
            }
            Some(Ok(status)) => {
                *h.exit_code.write().await = status.code();
                let mut s = h.status.write().await;
                if *s == "running" {
                    *s = if status.success() {
                        "success".into()
                    } else {
                        "failed".into()
                    };
                }
                drop(s);
                *h.finished_at_unix_ms.write().await = Some(now_unix_ms());
            }
            Some(Err(e)) => {
                *h.status.write().await = "failed".into();
                *h.error.write().await = Some(format!("wait error: {e}"));
                *h.finished_at_unix_ms.write().await = Some(now_unix_ms());
            }
        }
    });

    (StatusCode::ACCEPTED, Json(StartResponse { run_id })).into_response()
}

pub async fn get_run(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> axum::response::Response {
    require_auth!(headers, state);
    let handle = match state.shell_runs.read().await.get(&run_id).cloned() {
        Some(h) => h,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "run not found"})),
            )
                .into_response();
        }
    };
    let snap = RunSnapshot {
        run_id: handle.run_id.clone(),
        status: handle.status.read().await.clone(),
        exit_code: *handle.exit_code.read().await,
        stdout: handle.stdout.lock().await.snapshot_string(),
        stderr: handle.stderr.lock().await.snapshot_string(),
        started_at_unix_ms: handle.started_at_unix_ms,
        finished_at_unix_ms: *handle.finished_at_unix_ms.read().await,
        error: handle.error.read().await.clone(),
    };
    Json(snap).into_response()
}

pub async fn cancel_run(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> axum::response::Response {
    require_auth!(headers, state);
    let handle = match state.shell_runs.read().await.get(&run_id).cloned() {
        Some(h) => h,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "run not found"})),
            )
                .into_response();
        }
    };

    {
        let mut s = handle.status.write().await;
        if *s == "running" {
            *s = "cancelled".into();
        }
    }
    if let Some(pid) = handle.pid {
        send_sigterm(pid).await;
    }
    Json(serde_json::json!({"ok": true})).into_response()
}

/// Send SIGTERM to a pid by shelling out to `/bin/kill`. Avoids pulling in
/// libc just for this; on non-Unix the call is a no-op since the agent's
/// shell features are Linux-only in practice.
#[cfg(unix)]
async fn send_sigterm(pid: u32) {
    let _ = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .await;
}

#[cfg(not(unix))]
async fn send_sigterm(_pid: u32) {}

/// Background sweeper: drop runs whose `finished_at_unix_ms` is older than
/// 5 minutes. Running runs are always kept.
pub fn start_gc_loop(state: SharedState) {
    tokio::spawn(async move {
        loop {
            sleep(Duration::from_secs(60)).await;
            let cutoff = now_unix_ms() - 5 * 60 * 1000;
            let mut map = state.shell_runs.write().await;
            // Build the keep-set in a side pass so we can `await` the
            // per-entry locks without holding the iterator borrow.
            let mut to_drop: Vec<String> = Vec::new();
            for (id, h) in map.iter() {
                let finished = *h.finished_at_unix_ms.read().await;
                if let Some(ts) = finished {
                    if ts < cutoff {
                        to_drop.push(id.clone());
                    }
                }
            }
            for id in to_drop {
                map.remove(&id);
            }
        }
    });
}
