//! System logs (journalctl) — snapshot HTTP endpoint + WebSocket live tail.
//!
//! Exposes logs for a tightly allowlisted set of systemd units (pier, pier-agent)
//! through the admin UI so operators can debug runtime/startup issues without SSH.
//!
//! The systemd unit must run with `SupplementaryGroups=systemd-journal adm` for
//! the `pier` user to read its own journal — this is wired in `scripts/pier.service`
//! and `scripts/install.sh`.

use std::process::Stdio;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::{interval, Duration};

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// Systemd units the Logs UI is allowed to query. Anything outside this list
/// is rejected at the API layer.
pub const ALLOWED_UNITS: &[&str] = &["pier", "pier-agent"];

fn since_flag(preset: &str) -> Option<&'static str> {
    match preset {
        "5min" => Some("5 min ago"),
        "15min" => Some("15 min ago"),
        "1h" => Some("1 hour ago"),
        "6h" => Some("6 hours ago"),
        "24h" => Some("1 day ago"),
        "7d" => Some("7 days ago"),
        "all" => Some("1970-01-01"),
        _ => None,
    }
}

fn validate_lines(n: u32) -> Option<u32> {
    matches!(n, 30 | 100 | 500 | 1000 | 5000).then_some(n)
}

fn priority_flag(p: &str) -> Option<&'static str> {
    match p {
        "err" => Some("err"),
        "warning" => Some("warning"),
        "info" => Some("info"),
        "debug" => Some("debug"),
        _ => None,
    }
}

fn validate_unit(unit: &str) -> AppResult<&'static str> {
    for u in ALLOWED_UNITS {
        if *u == unit {
            return Ok(*u);
        }
    }
    Err(AppError::BadRequest(crate::i18n::te_args(
        "errors.system_logs.unit_not_allowed",
        &[("v", unit)],
    )))
}

#[derive(Deserialize)]
pub struct SnapshotQuery {
    pub unit: String,
    #[serde(default = "default_since")]
    pub since: String,
    #[serde(default = "default_lines")]
    pub lines: u32,
    #[serde(default)]
    pub priority: Option<String>,
}

fn default_since() -> String {
    "5min".to_string()
}
fn default_lines() -> u32 {
    100
}

#[derive(Deserialize)]
pub struct WsQuery {
    pub unit: String,
}

/// GET /api/v1/system/logs/units — installed allow-listed units.
pub async fn units_list(State(_state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let mut available = Vec::new();
    for unit in ALLOWED_UNITS {
        let out = Command::new("systemctl")
            .args([
                "list-unit-files",
                &format!("{unit}.service"),
                "--no-legend",
                "--no-pager",
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("systemctl: {e}"))?;

        let stdout = String::from_utf8_lossy(&out.stdout);
        if out.status.success() && !stdout.trim().is_empty() {
            available.push(*unit);
        }
    }
    Ok(Json(serde_json::json!({ "units": available })))
}

/// GET /api/v1/system/logs?unit=pier&since=5min&lines=100&priority=err
pub async fn snapshot(
    State(_state): State<SharedState>,
    Query(q): Query<SnapshotQuery>,
) -> AppResult<impl IntoResponse> {
    let unit = validate_unit(&q.unit)?;
    let since = since_flag(&q.since).ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.system_logs.invalid_since",
            &[("v", &q.since)],
        ))
    })?;
    let lines = validate_lines(q.lines).ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.system_logs.invalid_lines",
            &[("v", &q.lines.to_string())],
        ))
    })?;

    let lines_str = lines.to_string();
    let mut args: Vec<&str> = vec![
        "-u",
        unit,
        "--since",
        since,
        "-n",
        &lines_str,
        "--no-pager",
        "-o",
        "cat",
    ];
    if let Some(p) = q.priority.as_deref() {
        let pri = priority_flag(p).ok_or_else(|| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.system_logs.invalid_priority",
                &[("v", p)],
            ))
        })?;
        args.push("-p");
        args.push(pri);
    }

    let out = Command::new("journalctl")
        .args(&args)
        .output()
        .await
        .map_err(|e| anyhow::anyhow!("spawn journalctl: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

    if !out.status.success() {
        return Err(AppError::Internal(anyhow::anyhow!(
            "journalctl exit {}: {stderr}",
            out.status.code().unwrap_or(-1)
        )));
    }

    let lines_vec: Vec<String> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    Ok(Json(serde_json::json!({
        "unit": unit,
        "since": q.since,
        "requested_lines": lines,
        "count": lines_vec.len(),
        "lines": lines_vec,
    })))
}

/// GET /api/v1/system/logs/ws?unit=pier — live tail via WebSocket.
pub async fn stream_ws(
    State(_state): State<SharedState>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> AppResult<axum::response::Response> {
    let unit = validate_unit(&q.unit)?;
    Ok(ws.on_upgrade(move |socket| async move {
        stream_journal(unit, socket).await;
    }))
}

async fn stream_journal(unit: &'static str, mut socket: WebSocket) {
    let mut child = match Command::new("journalctl")
        .args(["-fu", unit, "--no-pager", "-n", "0", "-o", "cat"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    format!("[pier] failed to spawn journalctl: {e}").into(),
                ))
                .await;
            return;
        }
    };

    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill().await;
            return;
        }
    };
    let mut reader = BufReader::new(stdout).lines();
    let mut ping_tick = interval(Duration::from_secs(30));
    ping_tick.tick().await; // skip immediate fire

    loop {
        tokio::select! {
            line = reader.next_line() => {
                match line {
                    Ok(Some(text)) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break, // journalctl exited
                    Err(e) => {
                        tracing::warn!("journal stream read error for {unit}: {e}");
                        break;
                    }
                }
            }
            _ = ping_tick.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    break;
                }
            }
            msg = socket.recv() => {
                match msg {
                    None => break,
                    Some(Ok(Message::Close(_))) => break,
                    Some(Err(_)) => break,
                    _ => {}
                }
            }
        }
    }

    let _ = child.kill().await;
}
