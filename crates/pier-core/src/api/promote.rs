//! Promotion bundle — Mode 3 (pier-agent → pier-core).
//!
//! For a given `server_id` (typically a remote agent), collects every row
//! this Core knows about that relates to that server, plus global configuration
//! tables whose data the standalone instance will need, and serializes it as
//! a single JSON document.
//!
//! The bundle is designed to be fed into a fresh `pier-core` via the
//! `--import-bundle` CLI flag, giving the newly-promoted instance the same
//! view it had as an agent — minus users/sessions (so the new operator
//! creates their own admin) and minus federation state.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Debug, Serialize, Deserialize)]
pub struct PromoteBundle {
    pub schema_version: u32,
    pub pier_version: String,
    pub exported_at: String,
    pub source_server_id: String,
    pub source_server_name: String,
    /// Each entry: `table_name` → list of rows (row = JSON object, keys = column names).
    /// Order within the top-level map is significant: tables referenced by foreign keys
    /// come before their referents. `serde_json::Map` preserves insertion order.
    pub tables: Map<String, Value>,
}

/// Current bundle schema version. Bumped when the set of tables or column
/// semantics changes in a way the importer must detect.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// GET /api/v1/servers/{id}/promote-bundle — build + return the bundle as JSON.
pub async fn bundle(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (server_name, is_local): (String, bool) = db
        .query_row(
            "SELECT name, is_local FROM servers WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.promote.server_not_found",
                &[("v", &id)],
            ))
        })?;
    if is_local {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.promote.local_server_full_core",
        )));
    }

    let bundle = build_bundle(&db, &id, &server_name)?;
    Ok(Json(bundle))
}

#[derive(Deserialize, Default)]
pub struct PromoteRequest {
    #[serde(default)]
    pub core_download_url: Option<String>,
    #[serde(default)]
    pub core_port: Option<u16>,
    /// If true, remove the server from the local `servers` table after the
    /// agent accepted the promotion — the server will continue with its own
    /// pier-core panel and this Core will no longer know about it.
    #[serde(default = "default_true")]
    pub detach_after: bool,
}

fn default_true() -> bool {
    true
}

/// POST /api/v1/servers/{id}/promote — build bundle, push to agent, optionally detach.
pub async fn trigger(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<PromoteRequest>,
) -> AppResult<impl IntoResponse> {
    // 1. Resolve server connection info + build bundle under a scoped lock.
    let (bundle, host, port, agent_token, tls_fingerprint) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

        let (name, host, port, agent_token, is_local, tls_fingerprint) = db
            .query_row(
                "SELECT name, host, port, agent_token, is_local, agent_tls_fingerprint
                 FROM servers WHERE id = ?1",
                [&id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, bool>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .map_err(|_| {
                AppError::NotFound(crate::i18n::te_args(
                    "errors.promote.server_not_found",
                    &[("id", &id)],
                ))
            })?;
        if is_local {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.promote.local_server",
            )));
        }
        let bundle = build_bundle(&db, &id, &name)?;
        (bundle, host, port, agent_token, tls_fingerprint)
    };

    // 2. POST to the agent.
    let url = format!(
        "https://{}/api/v1/agent/promote",
        crate::network::address::authority(&host, port)
    );
    let payload = serde_json::json!({
        "bundle": bundle,
        "core_download_url": body.core_download_url,
        "core_port": body.core_port,
    });
    let client = crate::network::agent_client::build_agent_client(
        tls_fingerprint.as_deref(),
        std::time::Duration::from_secs(60),
    )
    .map_err(AppError::Internal)?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {agent_token}"))
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.promote.agent_unreachable",
                &[("v", &e.to_string())],
            ))
        })?;

    let status = resp.status();
    let body_val: serde_json::Value = resp.json().await.unwrap_or(serde_json::json!({}));
    if !status.is_success() {
        let err = body_val
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("agent rejected promotion")
            .to_string();
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.promote.agent_rejected",
            &[("v", &err)],
        )));
    }

    // 3. Optionally detach — drop the server record so this Core stops managing it.
    //    Deployed containers on the remote host continue running; the new pier-core
    //    will own them via the imported bundle.
    if body.detach_after {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.execute("DELETE FROM servers WHERE id = ?1 AND is_local = 0", [&id])?;
    }

    Ok(Json(serde_json::json!({
        "ok": true,
        "detached": body.detach_after,
        "agent_response": body_val,
    })))
}

/// Build the bundle. Split out so the same logic can be reused by a future
/// `POST /servers/{id}/promote` endpoint that pushes the bundle to the agent.
pub fn build_bundle(
    db: &Connection,
    server_id: &str,
    server_name: &str,
) -> AppResult<PromoteBundle> {
    let mut tables: Map<String, Value> = Map::new();

    // ── Projects that contain services on this server (parents must come first). ──
    tables.insert(
        "projects".into(),
        dump_rows(
            db,
            "SELECT * FROM projects WHERE id IN (
                SELECT DISTINCT project_id FROM services WHERE server_id = ?1 AND project_id IS NOT NULL
            )",
            &[server_id],
        )?,
    );

    // ── Global configuration. Copied in full so the standalone has everything. ──
    for table in [
        "s3_storages",
        "git_sources",
        "source_repos",
        "registry_credentials",
        "networks",
        "notification_channels",
        "alert_rules",
    ] {
        tables.insert(
            table.into(),
            dump_rows(db, &format!("SELECT * FROM {table}"), &[])?,
        );
    }

    // ── Per-server entities. Order respects FKs. ──
    tables.insert(
        "services".into(),
        dump_rows(
            db,
            "SELECT * FROM services WHERE server_id = ?1",
            &[server_id],
        )?,
    );
    tables.insert(
        "service_replicas".into(),
        dump_rows(
            db,
            "SELECT * FROM service_replicas
             WHERE server_id = ?1
                OR service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "port_allocations".into(),
        dump_rows(
            db,
            "SELECT * FROM port_allocations
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "domains".into(),
        dump_rows(
            db,
            "SELECT * FROM domains
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "backup_schedules".into(),
        dump_rows(
            db,
            "SELECT * FROM backup_schedules
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "backups".into(),
        dump_rows(
            db,
            "SELECT * FROM backups
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "deployments".into(),
        dump_rows(
            db,
            "SELECT * FROM deployments
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "deployment_logs".into(),
        dump_rows(
            db,
            "SELECT * FROM deployment_logs
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );
    tables.insert(
        "database_credentials".into(),
        dump_rows(
            db,
            "SELECT * FROM database_credentials
             WHERE service_id IN (SELECT id FROM services WHERE server_id = ?1)",
            &[server_id],
        )?,
    );

    // Subset of settings that make sense to carry (whitelist — avoid dragging
    // infrastructure-specific keys like server.public_ip or cleanup schedules).
    tables.insert(
        "settings".into(),
        dump_rows(
            db,
            "SELECT * FROM settings WHERE key LIKE 'notifications.%' OR key LIKE 'alerts.%'",
            &[],
        )?,
    );

    Ok(PromoteBundle {
        schema_version: BUNDLE_SCHEMA_VERSION,
        pier_version: env!("CARGO_PKG_VERSION").to_string(),
        exported_at: chrono::Utc::now().to_rfc3339(),
        source_server_id: server_id.to_string(),
        source_server_name: server_name.to_string(),
        tables,
    })
}

/// Apply a previously-exported `PromoteBundle` to a freshly-initialised database.
///
/// Preconditions enforced here:
/// * Schema version of the bundle must match `BUNDLE_SCHEMA_VERSION`.
/// * The target DB must not already contain services (guards against importing
///   into a live instance and clobbering its data).
///
/// Rows are inserted with `INSERT OR IGNORE`; conflicts on primary keys or
/// unique indexes are skipped rather than aborting the whole import. A summary
/// of inserted/skipped counts is returned.
pub fn import_bundle(db: &Connection, bundle: &PromoteBundle) -> anyhow::Result<ImportSummary> {
    if bundle.schema_version != BUNDLE_SCHEMA_VERSION {
        anyhow::bail!(
            "bundle schema version {} does not match importer version {}",
            bundle.schema_version,
            BUNDLE_SCHEMA_VERSION
        );
    }

    let existing_services: i64 = db.query_row("SELECT COUNT(*) FROM services", [], |r| r.get(0))?;
    if existing_services > 0 {
        anyhow::bail!(
            "target database already contains {existing_services} services; \
             import-bundle refuses to run on a populated instance"
        );
    }

    let mut summary = ImportSummary::default();
    for (table, rows_value) in &bundle.tables {
        let rows = rows_value
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("table '{table}' is not a JSON array"))?;
        for row in rows {
            let obj = row
                .as_object()
                .ok_or_else(|| anyhow::anyhow!("row in '{table}' is not an object"))?;
            let inserted = insert_row(db, table, obj)?;
            *summary.per_table.entry(table.clone()).or_insert(0) += inserted as u64;
            summary.total_rows += inserted as u64;
        }
    }
    Ok(summary)
}

#[derive(Debug, Default, Serialize)]
pub struct ImportSummary {
    pub total_rows: u64,
    pub per_table: std::collections::BTreeMap<String, u64>,
}

/// Insert a single JSON row into `table` using INSERT OR IGNORE.
/// Returns the number of rows actually inserted (0 if the row was skipped).
fn insert_row(db: &Connection, table: &str, row: &Map<String, Value>) -> anyhow::Result<usize> {
    if row.is_empty() {
        return Ok(0);
    }
    let cols: Vec<&str> = row.keys().map(String::as_str).collect();
    let placeholders: Vec<String> = (1..=cols.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "INSERT OR IGNORE INTO {table} ({}) VALUES ({})",
        cols.iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(","),
        placeholders.join(",")
    );

    let params: Vec<Box<dyn rusqlite::ToSql>> = cols
        .iter()
        .map(|c| json_to_sql(&row[*c]))
        .collect::<anyhow::Result<_>>()?;
    let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let affected = db.execute(&sql, params_refs.as_slice())?;
    Ok(affected)
}

fn json_to_sql(v: &Value) -> anyhow::Result<Box<dyn rusqlite::ToSql>> {
    Ok(match v {
        Value::Null => Box::new(Option::<String>::None),
        Value::Bool(b) => Box::new(*b as i64),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Box::new(i)
            } else if let Some(f) = n.as_f64() {
                Box::new(f)
            } else {
                Box::new(n.to_string())
            }
        }
        Value::String(s) => {
            if let Some(b64) = s.strip_prefix("__blob_b64:") {
                use base64::Engine;
                Box::new(
                    base64::engine::general_purpose::STANDARD
                        .decode(b64)
                        .map_err(|e| anyhow::anyhow!("blob decode: {e}"))?,
                )
            } else {
                Box::new(s.clone())
            }
        }
        Value::Array(_) | Value::Object(_) => Box::new(serde_json::to_string(v)?),
    })
}

/// Run a SELECT and return each row as a JSON object keyed by column name.
/// Supports parameterised queries with string parameters (sufficient for our joins on server_id).
fn dump_rows(db: &Connection, sql: &str, params: &[&str]) -> AppResult<Value> {
    let mut stmt = db.prepare(sql)?;
    let column_names: Vec<String> = stmt.column_names().into_iter().map(String::from).collect();
    let params_dyn: Vec<&dyn rusqlite::ToSql> =
        params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
    let mut rows = stmt.query(params_dyn.as_slice())?;
    let mut out: Vec<Value> = Vec::new();
    while let Some(row) = rows.next()? {
        let mut obj = Map::new();
        for (idx, name) in column_names.iter().enumerate() {
            let v = match row.get_ref(idx)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(i) => Value::Number(i.into()),
                ValueRef::Real(f) => serde_json::Number::from_f64(f)
                    .map(Value::Number)
                    .unwrap_or(Value::Null),
                ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
                ValueRef::Blob(b) => {
                    use base64::Engine;
                    Value::String(format!(
                        "__blob_b64:{}",
                        base64::engine::general_purpose::STANDARD.encode(b)
                    ))
                }
            };
            obj.insert(name.clone(), v);
        }
        out.push(Value::Object(obj));
    }
    Ok(Value::Array(out))
}
