//! In-panel database browser (Phase 1: PostgreSQL, read-only).
//!
//! Unlike [`super::databases`], which `docker exec`s the CLI (`psql`) inside
//! the container, this module connects to the database **over TCP** with a
//! native driver (`sqlx`). The DB container binds `127.0.0.1:{host_port}` and
//! pier-core runs natively on the same host, so a loopback connection reaches
//! it directly. Native typed results give us a clean, paginated data grid.
//!
//! Everything here is read-only (`SELECT` / catalog introspection) and gated
//! at `ProjectRole::Viewer`. The SQL runner (writes) lands in Phase 2 behind a
//! stricter gate.
//!
//! Identifier safety: schema/table names arrive as query params, so before any
//! string-built SQL touches them we confirm the `(schema, table)` pair exists
//! via a *parameterized* `information_schema` lookup, then double-quote every
//! identifier ([`quote_ident`]). Column names always come from the catalog,
//! never from the client.

use std::collections::{HashMap, HashSet};

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgSslMode};
use sqlx::{Connection, Row};

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// Resolved connection target for a PostgreSQL service.
struct PgTarget {
    host_port: u16,
    user: String,
    password: String,
    default_db: String,
}

/// Quote a SQL identifier by wrapping it in double quotes and doubling any
/// embedded quote. The value is still validated to exist in the catalog before
/// it reaches here, but quoting is the belt-and-suspenders that makes a stray
/// `"` harmless.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// Map a sqlx error to a user-facing `BadRequest`. The data browser is an
/// interactive tool, so surfacing the engine's message (permission denied,
/// undefined table, …) is the useful behaviour, not a generic 500.
fn db_err(e: sqlx::Error) -> AppError {
    AppError::BadRequest(format!("Query failed: {e}"))
}

/// Look up the service, confirm it's a PostgreSQL-family catalog item, and
/// assemble the loopback connection target from its allocated host port and
/// decrypted env (`POSTGRES_USER` / `POSTGRES_PASSWORD` / `POSTGRES_DB`).
fn resolve_pg_target(state: &SharedState, id: &str) -> AppResult<PgTarget> {
    let (catalog_id, env_json): (Option<String>, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, env_json FROM services WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let catalog = catalog_id.unwrap_or_default();
    if !matches!(catalog.as_str(), "postgresql" | "postgis" | "timescaledb") {
        return Err(AppError::BadRequest(
            "The data browser currently supports PostgreSQL only. MySQL, MongoDB and Redis are coming in later phases.".into(),
        ));
    }

    let env: HashMap<String, String> =
        serde_json::from_str(&crate::crypto::decrypt_env_json(env_json.as_deref()))
            .unwrap_or_default();
    let user = env
        .get("POSTGRES_USER")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "postgres".into());
    let password = env.get("POSTGRES_PASSWORD").cloned().unwrap_or_default();
    let default_db = env
        .get("POSTGRES_DB")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| user.clone());

    // Prefer the canonical 5432 mapping; fall back to whatever single port the
    // service exposes (covers non-default container ports).
    let host_port: i64 = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host_port FROM port_allocations WHERE service_id = ?1 AND container_port = 5432 LIMIT 1",
            [id],
            |row| row.get(0),
        )
        .or_else(|_| {
            db.query_row(
                "SELECT host_port FROM port_allocations WHERE service_id = ?1 ORDER BY host_port LIMIT 1",
                [id],
                |row| row.get(0),
            )
        })
        .map_err(|_| {
            AppError::BadRequest(
                "This PostgreSQL service has no host port allocated, so the panel can't reach it.".into(),
            )
        })?
    };

    Ok(PgTarget {
        host_port: host_port as u16,
        user,
        password,
        default_db,
    })
}

/// Open a loopback connection to `database` and cap statement time so a heavy
/// scan can't wedge a worker.
async fn connect(target: &PgTarget, database: &str) -> AppResult<PgConnection> {
    let opts = PgConnectOptions::new()
        .host("127.0.0.1")
        .port(target.host_port)
        .username(&target.user)
        .password(&target.password)
        .database(database)
        .ssl_mode(PgSslMode::Disable);

    let mut conn = PgConnection::connect_with(&opts)
        .await
        .map_err(|e| AppError::BadRequest(format!("Could not connect to PostgreSQL: {e}")))?;

    let _ = sqlx::query("SET statement_timeout = 15000")
        .execute(&mut conn)
        .await;

    Ok(conn)
}

/// Confirm a `(schema, table)` pair exists via a parameterized lookup. Returns
/// `NotFound` otherwise. Run this before building any SQL that interpolates the
/// identifiers.
async fn ensure_table_exists(conn: &mut PgConnection, schema: &str, table: &str) -> AppResult<()> {
    let found = sqlx::query(
        "SELECT 1 FROM information_schema.tables WHERE table_schema = $1 AND table_name = $2",
    )
    .bind(schema)
    .bind(table)
    .fetch_optional(conn)
    .await
    .map_err(db_err)?;

    if found.is_none() {
        return Err(AppError::NotFound(format!(
            "Table {schema}.{table} not found"
        )));
    }
    Ok(())
}

/// Query params shared by the browser endpoints.
#[derive(Deserialize)]
pub struct BrowseQuery {
    pub database: Option<String>,
    pub schema: Option<String>,
    pub table: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// GET /api/v1/resources/{id}/db-browser/databases — list non-template DBs.
pub async fn list_databases(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = resolve_pg_target(&state, &id)?;
    let mut conn = connect(&target, &target.default_db).await?;

    let rows =
        sqlx::query("SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname")
            .fetch_all(&mut conn)
            .await
            .map_err(db_err)?;

    let databases: Vec<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();

    Ok(Json(serde_json::json!({
        "databases": databases,
        "default": target.default_db,
    })))
}

/// GET /api/v1/resources/{id}/db-browser/objects?database=… — schemas + tables.
pub async fn objects(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = resolve_pg_target(&state, &id)?;
    let database = q.database.unwrap_or_else(|| target.default_db.clone());
    let mut conn = connect(&target, &database).await?;

    let rows = sqlx::query(
        "SELECT table_schema, table_name, table_type
         FROM information_schema.tables
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema')
         ORDER BY table_schema, table_name",
    )
    .fetch_all(&mut conn)
    .await
    .map_err(db_err)?;

    // Group tables under their schema, preserving first-seen order.
    let mut schemas: Vec<serde_json::Value> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let table_type: String = row.get(2);
        let kind = if table_type == "VIEW" {
            "view"
        } else {
            "table"
        };

        let i = match index.get(&schema) {
            Some(&i) => i,
            None => {
                let i = schemas.len();
                schemas.push(serde_json::json!({ "name": schema, "tables": [] }));
                index.insert(schema.clone(), i);
                i
            }
        };
        if let Some(arr) = schemas[i]["tables"].as_array_mut() {
            arr.push(serde_json::json!({ "name": name, "kind": kind }));
        }
    }

    Ok(Json(serde_json::json!({
        "database": database,
        "schemas": schemas,
    })))
}

/// GET /api/v1/resources/{id}/db-browser/structure?database=&schema=&table=
pub async fn structure(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = resolve_pg_target(&state, &id)?;
    let database = q.database.unwrap_or_else(|| target.default_db.clone());
    let schema = q
        .schema
        .ok_or_else(|| AppError::BadRequest("schema is required".into()))?;
    let table = q
        .table
        .ok_or_else(|| AppError::BadRequest("table is required".into()))?;

    let mut conn = connect(&target, &database).await?;
    ensure_table_exists(&mut conn, &schema, &table).await?;

    let pk_rows = sqlx::query(
        "SELECT kcu.column_name
         FROM information_schema.table_constraints tc
         JOIN information_schema.key_column_usage kcu
           ON tc.constraint_name = kcu.constraint_name
          AND tc.table_schema = kcu.table_schema
         WHERE tc.constraint_type = 'PRIMARY KEY'
           AND tc.table_schema = $1 AND tc.table_name = $2",
    )
    .bind(&schema)
    .bind(&table)
    .fetch_all(&mut conn)
    .await
    .map_err(db_err)?;
    let pks: HashSet<String> = pk_rows.iter().map(|r| r.get::<String, _>(0)).collect();

    let col_rows = sqlx::query(
        "SELECT column_name, data_type, is_nullable, column_default
         FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2
         ORDER BY ordinal_position",
    )
    .bind(&schema)
    .bind(&table)
    .fetch_all(&mut conn)
    .await
    .map_err(db_err)?;

    let columns: Vec<serde_json::Value> = col_rows
        .iter()
        .map(|r| {
            let name: String = r.get(0);
            let is_pk = pks.contains(&name);
            serde_json::json!({
                "name": name,
                "type": r.get::<String, _>(1),
                "nullable": r.get::<String, _>(2) == "YES",
                "default": r.get::<Option<String>, _>(3),
                "is_pk": is_pk,
            })
        })
        .collect();

    let idx_rows = sqlx::query(
        "SELECT indexname, indexdef FROM pg_indexes
         WHERE schemaname = $1 AND tablename = $2 ORDER BY indexname",
    )
    .bind(&schema)
    .bind(&table)
    .fetch_all(&mut conn)
    .await
    .map_err(db_err)?;
    let indexes: Vec<serde_json::Value> = idx_rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.get::<String, _>(0),
                "def": r.get::<String, _>(1),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        "schema": schema,
        "table": table,
        "columns": columns,
        "indexes": indexes,
    })))
}

/// GET /api/v1/resources/{id}/db-browser/rows?database=&schema=&table=&limit=&offset=
///
/// Every column is cast to `::text` so arbitrary types collapse to strings (or
/// `null`) for a uniform grid. This covers the common types (text, numeric,
/// timestamps, bool, json/jsonb, uuid, arrays); exotic types without a text
/// cast will surface the engine error, which is acceptable for a browser.
pub async fn rows(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = resolve_pg_target(&state, &id)?;
    let database = q.database.unwrap_or_else(|| target.default_db.clone());
    let schema = q
        .schema
        .ok_or_else(|| AppError::BadRequest("schema is required".into()))?;
    let table = q
        .table
        .ok_or_else(|| AppError::BadRequest("table is required".into()))?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let mut conn = connect(&target, &database).await?;
    ensure_table_exists(&mut conn, &schema, &table).await?;

    // Ordered column names straight from the catalog (never client input).
    let col_rows = sqlx::query(
        "SELECT column_name FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
    )
    .bind(&schema)
    .bind(&table)
    .fetch_all(&mut conn)
    .await
    .map_err(db_err)?;
    let cols: Vec<String> = col_rows.iter().map(|r| r.get::<String, _>(0)).collect();

    if cols.is_empty() {
        return Ok(Json(serde_json::json!({
            "columns": [], "rows": [], "total": 0, "limit": limit, "offset": offset,
        })));
    }

    let select_list = cols
        .iter()
        .map(|c| format!("{}::text", quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let qualified = format!("{}.{}", quote_ident(&schema), quote_ident(&table));
    let sql = format!("SELECT {select_list} FROM {qualified} LIMIT $1 OFFSET $2");

    let data_rows = sqlx::query(&sql)
        .bind(limit)
        .bind(offset)
        .fetch_all(&mut conn)
        .await
        .map_err(db_err)?;

    let grid: Vec<Vec<Option<String>>> = data_rows
        .iter()
        .map(|row| {
            (0..cols.len())
                .map(|i| row.try_get::<Option<String>, _>(i).unwrap_or(None))
                .collect()
        })
        .collect();

    // Exact count, best-effort — may hit the statement timeout on huge tables,
    // in which case we report null rather than failing the whole request.
    let count_sql = format!("SELECT count(*)::bigint FROM {qualified}");
    let total: Option<i64> = sqlx::query(&count_sql)
        .fetch_one(&mut conn)
        .await
        .ok()
        .map(|r| r.get::<i64, _>(0));

    Ok(Json(serde_json::json!({
        "columns": cols,
        "rows": grid,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}
