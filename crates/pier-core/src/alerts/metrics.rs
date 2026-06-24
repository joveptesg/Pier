use std::time::Duration;

use chrono::{DateTime, Utc};
use sysinfo::System;

use crate::state::SharedState;

/// Fetch a numeric metric for a given rule. Returns None if metric is event-based
/// or if fetching fails.
pub async fn fetch(
    state: &SharedState,
    metric: &str,
    scope: &str,
    scope_id: Option<&str>,
) -> Option<f64> {
    match metric {
        "cpu" | "ram" | "disk" => {
            if scope == "server" {
                fetch_server_host_metric(state, metric, scope_id?).await
            } else {
                fetch_local_host_metric(metric)
            }
        }
        "agent_offline" => fetch_agent_offline_minutes(state, scope_id?).await,
        "container_cpu" | "container_ram" => fetch_container_metric(state, metric, scope_id?).await,
        "ssl_expiry" => fetch_ssl_days_left(state, scope_id).await,
        _ => None,
    }
}

fn fetch_local_host_metric(metric: &str) -> Option<f64> {
    let mut sys = System::new_all();
    sys.refresh_all();

    match metric {
        "cpu" => {
            let count = sys.cpus().len().max(1) as f32;
            let usage = sys.cpus().iter().map(|c| c.cpu_usage()).sum::<f32>() / count;
            Some(usage as f64)
        }
        "ram" => {
            let total = sys.total_memory() as f64;
            let used = sys.used_memory() as f64;
            if total > 0.0 {
                Some((used / total) * 100.0)
            } else {
                None
            }
        }
        "disk" => {
            let disks = sysinfo::Disks::new_with_refreshed_list();
            let worst = disks
                .iter()
                .filter(|d| d.total_space() > 0)
                .map(|d| {
                    let used = d.total_space().saturating_sub(d.available_space());
                    (used as f64 / d.total_space() as f64) * 100.0
                })
                .fold(0.0_f64, f64::max);
            Some(worst)
        }
        _ => None,
    }
}

async fn fetch_server_host_metric(
    state: &SharedState,
    metric: &str,
    server_id: &str,
) -> Option<f64> {
    // If it's the local server, use sysinfo directly
    let (is_local, host, port, token, tls_fingerprint) = {
        let db = state.db.lock().ok()?;
        db.query_row(
            "SELECT is_local, host, port, agent_token, agent_tls_fingerprint FROM servers WHERE id = ?1",
            [server_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? == 1,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .ok()?
    };

    if is_local {
        return fetch_local_host_metric(metric);
    }

    // Remote: call agent /metrics (pinned to the agent's leaf fingerprint).
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        Duration::from_secs(5),
    )
    .ok()?;
    let url = format!(
        "https://{}/metrics",
        crate::network::address::authority(&host, port)
    );
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .ok()?;
    let json: serde_json::Value = resp.json().await.ok()?;

    match metric {
        "cpu" => json
            .get("cpu_usage")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .or_else(|| json.get("cpu_usage").and_then(|v| v.as_f64())),
        "ram" => json
            .get("memory_percent")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse().ok())
            .or_else(|| json.get("memory_percent").and_then(|v| v.as_f64())),
        "disk" => json.get("disks").and_then(|v| v.as_array()).map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    let total = d.get("total")?.as_u64()? as f64;
                    let avail = d.get("available")?.as_u64()? as f64;
                    if total > 0.0 {
                        Some(((total - avail) / total) * 100.0)
                    } else {
                        None
                    }
                })
                .fold(0.0_f64, f64::max)
        }),
        _ => None,
    }
}

async fn fetch_agent_offline_minutes(state: &SharedState, server_id: &str) -> Option<f64> {
    let hb: Option<String> = {
        let db = state.db.lock().ok()?;
        db.query_row(
            "SELECT last_heartbeat FROM servers WHERE id = ?1 AND is_local = 0",
            [server_id],
            |row| row.get(0),
        )
        .ok()?
    };
    let hb = hb?;
    let parsed = parse_sqlite_ts(&hb)?;
    let diff = (Utc::now() - parsed).num_seconds().max(0) as f64 / 60.0;
    Some(diff)
}

async fn fetch_container_metric(
    state: &SharedState,
    metric: &str,
    service_id: &str,
) -> Option<f64> {
    let container_id: Option<String> = {
        let db = state.db.lock().ok()?;
        db.query_row(
            "SELECT container_id FROM services WHERE id = ?1",
            [service_id],
            |row| row.get(0),
        )
        .ok()?
    };
    let cid = container_id.filter(|s| !s.is_empty())?;
    let stats = crate::docker::containers::container_stats(&state.docker, &cid)
        .await
        .ok()?;

    match metric {
        "container_cpu" => stats.get("cpu_percent").and_then(|v| v.as_f64()),
        "container_ram" => stats.get("memory_percent").and_then(|v| v.as_f64()),
        _ => None,
    }
}

/// Returns minimum days-until-expiry across the given scope.
/// If scope_id is a domain id — that single domain. If None — global min across all domains.
async fn fetch_ssl_days_left(state: &SharedState, scope_id: Option<&str>) -> Option<f64> {
    let rows: Vec<Option<String>> = {
        let db = state.db.lock().ok()?;
        match scope_id {
            Some(id) => {
                let v: Option<String> = db
                    .query_row(
                        "SELECT ssl_expires_at FROM domains WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )
                    .ok()?;
                vec![v]
            }
            None => {
                let mut stmt = db
                    .prepare("SELECT ssl_expires_at FROM domains WHERE ssl_expires_at IS NOT NULL")
                    .ok()?;
                let rows: Vec<Option<String>> = stmt
                    .query_map([], |row| row.get::<_, Option<String>>(0))
                    .ok()?
                    .filter_map(|r| r.ok())
                    .collect();
                rows
            }
        }
    };

    let now = Utc::now();
    let min_days = rows
        .into_iter()
        .flatten()
        .filter_map(|s| parse_sqlite_ts(&s).or_else(|| parse_any_ts(&s)))
        .map(|t| (t - now).num_seconds() as f64 / 86400.0)
        .fold(f64::INFINITY, f64::min);

    if min_days.is_finite() {
        Some(min_days)
    } else {
        None
    }
}

fn parse_sqlite_ts(s: &str) -> Option<DateTime<Utc>> {
    let padded = format!("{s}+00:00");
    chrono::DateTime::parse_from_str(&padded, "%Y-%m-%d %H:%M:%S%:z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn parse_any_ts(s: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// Build "name (host)" for the host that owns the event.
///
/// - `scope=server` + id → that server
/// - `scope=service` + id → the service's owning server (via services.server_id)
/// - everything else → the local server (is_local = 1)
pub fn resolve_server_label(
    state: &SharedState,
    scope: &str,
    scope_id: Option<&str>,
) -> Option<String> {
    let db = state.db.lock().ok()?;

    if scope == "server" {
        if let Some(id) = scope_id {
            return db
                .query_row(
                    "SELECT name || ' (' || host || ')' FROM servers WHERE id = ?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .ok();
        }
    }

    if scope == "service" {
        if let Some(id) = scope_id {
            if let Ok(label) = db.query_row(
                "SELECT srv.name || ' (' || srv.host || ')'
                 FROM services s JOIN servers srv ON srv.id = s.server_id
                 WHERE s.id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            ) {
                return Some(label);
            }
        }
    }

    db.query_row(
        "SELECT name || ' (' || host || ')' FROM servers WHERE is_local = 1 LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Build a human-readable scope label for alert messages.
pub fn scope_label(state: &SharedState, scope: &str, scope_id: Option<&str>) -> String {
    match (scope, scope_id) {
        ("global", _) => "global".to_string(),
        ("server", Some(id)) => state
            .db
            .lock()
            .ok()
            .and_then(|db| {
                db.query_row(
                    "SELECT name || ' (' || host || ')' FROM servers WHERE id = ?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
            .unwrap_or_else(|| format!("server {id}")),
        ("service", Some(id)) => state
            .db
            .lock()
            .ok()
            .and_then(|db| {
                db.query_row("SELECT name FROM services WHERE id = ?1", [id], |row| {
                    row.get::<_, String>(0)
                })
                .ok()
            })
            .unwrap_or_else(|| format!("service {id}")),
        ("domain", Some(id)) => state
            .db
            .lock()
            .ok()
            .and_then(|db| {
                db.query_row("SELECT domain FROM domains WHERE id = ?1", [id], |row| {
                    row.get::<_, String>(0)
                })
                .ok()
            })
            .unwrap_or_else(|| format!("domain {id}")),
        (s, _) => s.to_string(),
    }
}
