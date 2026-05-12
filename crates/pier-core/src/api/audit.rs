//! `GET /api/v1/audit/events` — paginated, filterable view of `auth_events`.
//!
//! Filter parameters (all optional, AND-combined):
//!   - `event_type=login_failure,login_success` (comma-separated)
//!   - `user_id=<id>`
//!   - `ip=<prefix>`         — LIKE 'prefix%' so `ip=192.168` matches a subnet
//!   - `since=<RFC3339>`     — events strictly after this timestamp
//!   - `until=<RFC3339>`     — events strictly before this timestamp
//!   - `limit=<n>` (max 200, default 50)
//!   - `offset=<n>`
//!
//! Response: `{ "events": [...], "total": <int>, "limit": …, "offset": … }`.

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::AppResult;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct ListParams {
    #[serde(default)]
    pub event_type: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub until: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
    #[serde(default)]
    pub offset: Option<u32>,
}

pub async fn list_events(
    State(state): State<SharedState>,
    Query(p): Query<ListParams>,
) -> AppResult<impl IntoResponse> {
    let limit = p.limit.unwrap_or(50).clamp(1, 200);
    let offset = p.offset.unwrap_or(0);

    // We build the WHERE clause + parameter list dynamically. Each fragment
    // appends to both, so they stay in sync.
    let mut wheres: Vec<String> = Vec::new();
    let mut args: Vec<Box<dyn rusqlite::ToSql + Send>> = Vec::new();

    if let Some(types) = &p.event_type {
        let parts: Vec<&str> = types
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();
        if !parts.is_empty() {
            let placeholders = parts.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            wheres.push(format!("event_type IN ({placeholders})"));
            for t in parts {
                args.push(Box::new(t.to_string()));
            }
        }
    }
    if let Some(uid) = &p.user_id {
        if !uid.is_empty() {
            wheres.push("user_id = ?".into());
            args.push(Box::new(uid.clone()));
        }
    }
    if let Some(ip) = &p.ip {
        if !ip.is_empty() {
            wheres.push("ip LIKE ?".into());
            args.push(Box::new(format!("{ip}%")));
        }
    }
    if let Some(since) = &p.since {
        if !since.is_empty() {
            wheres.push("created_at > ?".into());
            args.push(Box::new(since.clone()));
        }
    }
    if let Some(until) = &p.until {
        if !until.is_empty() {
            wheres.push("created_at < ?".into());
            args.push(Box::new(until.clone()));
        }
    }

    let where_sql = if wheres.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", wheres.join(" AND "))
    };

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Total count for pagination UI.
    let count_sql = format!("SELECT COUNT(*) FROM auth_events {where_sql}");
    let params_ref: Vec<&dyn rusqlite::ToSql> = args
        .iter()
        .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
        .collect();
    let total: i64 = db.query_row(&count_sql, params_ref.as_slice(), |row| row.get(0))?;

    // Page query — append LIMIT/OFFSET as positional params at the end so we
    // can keep using the same `args` vector.
    let list_sql = format!(
        "SELECT e.id, e.user_id, u.username, e.event_type, e.ip, e.user_agent, e.details, e.created_at
         FROM auth_events e
         LEFT JOIN users u ON u.id = e.user_id
         {where_sql}
         ORDER BY e.created_at DESC
         LIMIT ? OFFSET ?"
    );
    let mut args2: Vec<Box<dyn rusqlite::ToSql + Send>> = args;
    args2.push(Box::new(limit as i64));
    args2.push(Box::new(offset as i64));
    let params_ref2: Vec<&dyn rusqlite::ToSql> = args2
        .iter()
        .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
        .collect();

    let mut stmt = db.prepare(&list_sql)?;
    let events: Vec<serde_json::Value> = stmt
        .query_map(params_ref2.as_slice(), |row| {
            Ok(serde_json::json!({
                "id":          row.get::<_, String>(0)?,
                "user_id":     row.get::<_, Option<String>>(1)?,
                "username":    row.get::<_, Option<String>>(2)?,
                "event_type":  row.get::<_, String>(3)?,
                "ip":          row.get::<_, Option<String>>(4)?,
                "user_agent":  row.get::<_, Option<String>>(5)?,
                "details":     row.get::<_, Option<String>>(6)?,
                "created_at":  row.get::<_, String>(7)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(serde_json::json!({
        "events": events,
        "total":  total,
        "limit":  limit,
        "offset": offset,
    })))
}
