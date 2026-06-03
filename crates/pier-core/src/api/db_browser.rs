//! In-panel database browser + SQL runner (Phase 2).
//!
//! Supports **PostgreSQL** and **MySQL/MariaDB** via `sqlx`. (The `Any` driver
//! is deliberately avoided: it pulls `sqlx-sqlite`, whose `libsqlite3-sys`
//! collides with the one `rusqlite` already links.) Instead a small [`Db`]
//! dispatch enum holds a concrete `PgConnection` or `MySqlConnection`, and a
//! single [`Db::fetch_text`] primitive returns every result as a grid of
//! `Option<String>` — so the handlers stay engine-agnostic.
//!
//! Unlike [`super::databases`] (which `docker exec`s the CLI), this connects to
//! the database **over TCP**. Private DB services aren't published to the host,
//! so we dial the container's `pier-net` IP + internal port ([`container_host`]),
//! falling back to `127.0.0.1:{host_port}` for published ports.
//!
//! Two surfaces:
//! - **Browser** (read-only, `Viewer+`): list databases, schema/table tree,
//!   table structure, paginated rows.
//! - **SQL runner** (`Editor+`): execute an arbitrary statement. Reads come
//!   back as a grid (wrapped so all columns are text); writes report the
//!   affected-row count. Every run is recorded in `db_query_log`.
//!
//! Identifier safety: schema/table names arrive as query params, so before any
//! string-built SQL touches them we confirm the pair exists via an
//! `information_schema` lookup, and every identifier/literal is engine-quoted
//! ([`Engine::quote_ident`] / [`Engine::quote_literal`]). Column names always
//! come from the catalog, never from the client. Bind placeholders differ
//! between backends (`$1` vs `?`), so we inline engine-quoted literals instead.

use std::collections::HashSet;
use std::time::Instant;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlRow, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgConnection, PgRow, PgSslMode};
use sqlx::{Column, Connection, Executor, Row, Statement};

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// Hard cap on rows returned by the SQL runner in a single read.
const READ_CAP: i64 = 1000;

#[derive(Copy, Clone, PartialEq, Eq)]
enum Engine {
    Postgres,
    Mysql,
}

impl Engine {
    fn from_catalog(c: &str) -> Option<Self> {
        match c {
            "postgresql" | "postgis" | "timescaledb" => Some(Engine::Postgres),
            "mysql" | "mariadb" => Some(Engine::Mysql),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::Mysql => "MySQL",
        }
    }

    fn default_port(self) -> i64 {
        match self {
            Engine::Postgres => 5432,
            Engine::Mysql => 3306,
        }
    }

    fn quote_ident(self, s: &str) -> String {
        match self {
            Engine::Postgres => format!("\"{}\"", s.replace('"', "\"\"")),
            Engine::Mysql => format!("`{}`", s.replace('`', "``")),
        }
    }

    /// Quote a string literal. MySQL also treats backslash as an escape in its
    /// default mode, so we double those too.
    fn quote_literal(self, s: &str) -> String {
        let mut out = s.replace('\'', "''");
        if self == Engine::Mysql {
            out = out.replace('\\', "\\\\");
        }
        format!("'{out}'")
    }

    /// Wrap `expr` in a cast to text so any column type renders as a string.
    fn text_cast(self, expr: &str) -> String {
        match self {
            Engine::Postgres => format!("({expr})::text"),
            Engine::Mysql => format!("CAST({expr} AS CHAR)"),
        }
    }
}

/// Resolved connection target for a SQL service. We prefer the container's
/// `pier-net` IP + internal port (works for unpublished DBs); `host_port` is the
/// loopback fallback for published ports.
struct Target {
    engine: Engine,
    container: String,
    container_port: u16,
    host_port: u16,
    user: String,
    password: String,
    default_db: String,
}

/// A live connection to whichever engine the service runs.
enum Db {
    Pg(PgConnection),
    My(MySqlConnection),
}

impl Db {
    /// Fetch every selected column of every row as text (or null). Callers must
    /// ensure the selected columns are text-typed (introspection columns are;
    /// data columns are cast via [`Engine::text_cast`]).
    async fn fetch_text(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>, sqlx::Error> {
        match self {
            Db::Pg(c) => Ok(grid_pg(&sqlx::query(sql).fetch_all(c).await?)),
            Db::My(c) => Ok(grid_my(&sqlx::query(sql).fetch_all(c).await?)),
        }
    }

    /// Execute a statement that returns no rowset; report affected rows.
    async fn exec(&mut self, sql: &str) -> Result<u64, sqlx::Error> {
        match self {
            Db::Pg(c) => Ok(sqlx::query(sql).execute(c).await?.rows_affected()),
            Db::My(c) => Ok(sqlx::query(sql).execute(c).await?.rows_affected()),
        }
    }

    /// Column names of a statement, learned by preparing it (no execution).
    async fn column_names(&mut self, sql: &str) -> Result<Vec<String>, sqlx::Error> {
        match self {
            Db::Pg(c) => {
                let st = c.prepare(sql).await?;
                Ok(st.columns().iter().map(|c| c.name().to_string()).collect())
            }
            Db::My(c) => {
                let st = c.prepare(sql).await?;
                Ok(st.columns().iter().map(|c| c.name().to_string()).collect())
            }
        }
    }
}

fn grid_pg(rows: &[PgRow]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| r.try_get::<Option<String>, _>(i).unwrap_or(None))
                .collect()
        })
        .collect()
}

fn grid_my(rows: &[MySqlRow]) -> Vec<Vec<Option<String>>> {
    rows.iter()
        .map(|r| {
            (0..r.columns().len())
                .map(|i| r.try_get::<Option<String>, _>(i).unwrap_or(None))
                .collect()
        })
        .collect()
}

fn db_err(e: sqlx::Error) -> AppError {
    AppError::BadRequest(format!("Query failed: {e}"))
}

/// Look up the service, classify its engine, and assemble the connection target
/// from the allocated host port + decrypted env credentials.
fn resolve_target(state: &SharedState, id: &str) -> AppResult<Target> {
    let (catalog_id, name, env_json): (Option<String>, String, Option<String>) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, name, env_json FROM services WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let catalog = catalog_id.unwrap_or_default();
    let engine = Engine::from_catalog(&catalog).ok_or_else(|| {
        AppError::BadRequest(
            "The data browser supports PostgreSQL and MySQL/MariaDB. MongoDB and Redis are coming in a later phase.".into(),
        )
    })?;

    let env: std::collections::HashMap<String, String> =
        serde_json::from_str(&crate::crypto::decrypt_env_json(env_json.as_deref()))
            .unwrap_or_default();
    let nonempty = |s: &String| !s.is_empty();

    let (user, password, default_db) = match engine {
        Engine::Postgres => {
            let user = env
                .get("POSTGRES_USER")
                .cloned()
                .filter(nonempty)
                .unwrap_or_else(|| "postgres".into());
            let password = env.get("POSTGRES_PASSWORD").cloned().unwrap_or_default();
            let default_db = env
                .get("POSTGRES_DB")
                .cloned()
                .filter(nonempty)
                .unwrap_or_else(|| user.clone());
            (user, password, default_db)
        }
        Engine::Mysql => {
            let password = env
                .get("MYSQL_ROOT_PASSWORD")
                .or_else(|| env.get("MARIADB_ROOT_PASSWORD"))
                .cloned()
                .unwrap_or_default();
            let default_db = env
                .get("MYSQL_DATABASE")
                .or_else(|| env.get("MARIADB_DATABASE"))
                .cloned()
                .filter(nonempty)
                .unwrap_or_else(|| "information_schema".into());
            ("root".into(), password, default_db)
        }
    };

    let host_port: i64 = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT host_port FROM port_allocations WHERE service_id = ?1 AND container_port = ?2 LIMIT 1",
            rusqlite::params![id, engine.default_port()],
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
            AppError::BadRequest(format!(
                "This {} service has no host port allocated, so the panel can't reach it.",
                engine.label()
            ))
        })?
    };

    Ok(Target {
        engine,
        container: format!("pier-{}", name.to_lowercase().replace(' ', "-")),
        container_port: engine.default_port() as u16,
        host_port: host_port as u16,
        user,
        password,
        default_db,
    })
}

/// Resolve a container's reachable IP on the `pier-net` docker network (falling
/// back to the first network with an address). This lets a host-native pier-core
/// reach a DB container whose port isn't published to the host loopback.
pub(crate) async fn container_host(docker: &bollard::Docker, container: &str) -> Option<String> {
    let info = docker.inspect_container(container, None).await.ok()?;
    let nets = info.network_settings?.networks?;
    if let Some(ep) = nets.get("pier-net") {
        if let Some(ip) = ep.ip_address.as_ref().filter(|ip| !ip.is_empty()) {
            return Some(ip.clone());
        }
    }
    nets.values()
        .find_map(|ep| ep.ip_address.as_ref().filter(|ip| !ip.is_empty()).cloned())
}

/// Open a connection to `database` and bound statement time. Prefers the
/// container's `pier-net` IP + internal port; falls back to `127.0.0.1:host_port`.
async fn connect(state: &SharedState, target: &Target, database: &str) -> AppResult<Db> {
    let (host, port) = match container_host(&state.docker, &target.container).await {
        Some(ip) => (ip, target.container_port),
        None => ("127.0.0.1".to_string(), target.host_port),
    };
    let conn_err = |e: sqlx::Error| {
        AppError::BadRequest(format!(
            "Could not connect to {} at {host}:{port}: {e}",
            target.engine.label()
        ))
    };
    match target.engine {
        Engine::Postgres => {
            let opts = PgConnectOptions::new()
                .host(&host)
                .port(port)
                .username(&target.user)
                .password(&target.password)
                .database(database)
                .ssl_mode(PgSslMode::Disable);
            let mut c = PgConnection::connect_with(&opts).await.map_err(conn_err)?;
            let _ = sqlx::query("SET statement_timeout = 15000")
                .execute(&mut c)
                .await;
            Ok(Db::Pg(c))
        }
        Engine::Mysql => {
            let opts = MySqlConnectOptions::new()
                .host(&host)
                .port(port)
                .username(&target.user)
                .password(&target.password)
                .database(database)
                .ssl_mode(MySqlSslMode::Disabled);
            let mut c = MySqlConnection::connect_with(&opts)
                .await
                .map_err(conn_err)?;
            let _ = sqlx::query("SET SESSION max_execution_time = 15000")
                .execute(&mut c)
                .await;
            Ok(Db::My(c))
        }
    }
}

/// Confirm a `(schema, table)` pair exists. Run before interpolating either
/// into string-built SQL.
async fn ensure_table_exists(
    db: &mut Db,
    engine: Engine,
    schema: &str,
    table: &str,
) -> AppResult<()> {
    let sql = format!(
        "SELECT table_name FROM information_schema.tables WHERE table_schema = {} AND table_name = {}",
        engine.quote_literal(schema),
        engine.quote_literal(table),
    );
    let found = db.fetch_text(&sql).await.map_err(db_err)?;
    if found.is_empty() {
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
    /// Column to sort by. Validated against the table's catalog columns before
    /// use, so it can never be arbitrary SQL.
    pub sort: Option<String>,
    /// Sort direction — only `asc` / `desc` are honoured.
    pub dir: Option<String>,
    /// Single-column filter: column (validated against catalog), operator
    /// (fixed allowlist) and value (engine-quoted literal).
    pub filter_col: Option<String>,
    pub filter_op: Option<String>,
    pub filter_val: Option<String>,
}

/// Build a validated `WHERE` clause from a single-column filter, or empty string.
/// `col` must be one of the table's real columns; the operator is a fixed
/// allowlist; the value is engine-quoted. Returns `" WHERE …"` (leading space)
/// or `""`.
fn build_filter(
    engine: Engine,
    cols: &[String],
    filter_col: Option<&str>,
    filter_op: Option<&str>,
    filter_val: Option<&str>,
) -> String {
    let col = match filter_col {
        Some(c) if cols.iter().any(|x| x == c) => c,
        _ => return String::new(),
    };
    let ident = engine.quote_ident(col);
    let val = filter_val.unwrap_or_default();
    match filter_op.unwrap_or("eq") {
        "eq" => format!(" WHERE {ident} = {}", engine.quote_literal(val)),
        "ne" => format!(" WHERE {ident} <> {}", engine.quote_literal(val)),
        "gt" => format!(" WHERE {ident} > {}", engine.quote_literal(val)),
        "lt" => format!(" WHERE {ident} < {}", engine.quote_literal(val)),
        "ge" => format!(" WHERE {ident} >= {}", engine.quote_literal(val)),
        "le" => format!(" WHERE {ident} <= {}", engine.quote_literal(val)),
        // LIKE on text-cast so it works for any column type.
        "like" => format!(
            " WHERE {} LIKE {}",
            engine.text_cast(&ident),
            engine.quote_literal(val)
        ),
        "null" => format!(" WHERE {ident} IS NULL"),
        "notnull" => format!(" WHERE {ident} IS NOT NULL"),
        _ => String::new(),
    }
}

/// GET /api/v1/resources/{id}/db-browser/databases — list user-visible DBs.
pub async fn list_databases(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = resolve_target(&state, &id)?;
    let mut db = connect(&state, &target, &target.default_db).await?;

    let sql = match target.engine {
        Engine::Postgres => {
            "SELECT datname FROM pg_database WHERE datistemplate = false ORDER BY datname"
        }
        Engine::Mysql => {
            "SELECT schema_name FROM information_schema.schemata \
             WHERE schema_name NOT IN ('mysql', 'performance_schema', 'information_schema', 'sys') \
             ORDER BY schema_name"
        }
    };
    let grid = db.fetch_text(sql).await.map_err(db_err)?;
    let databases: Vec<String> = grid
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .collect();

    Ok(Json(serde_json::json!({
        "databases": databases,
        "default": target.default_db,
        "engine": target.engine.label(),
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
    let target = resolve_target(&state, &id)?;
    let database = q.database.unwrap_or_else(|| target.default_db.clone());
    let mut db = connect(&state, &target, &database).await?;

    // Postgres: every user schema in the connected DB. MySQL: a database *is* a
    // schema, so scope to the connected one.
    let filter = match target.engine {
        Engine::Postgres => "table_schema NOT IN ('pg_catalog', 'information_schema')".to_string(),
        Engine::Mysql => format!("table_schema = {}", target.engine.quote_literal(&database)),
    };
    let sql = format!(
        "SELECT table_schema, table_name, table_type FROM information_schema.tables \
         WHERE {filter} ORDER BY table_schema, table_name"
    );
    let grid = db.fetch_text(&sql).await.map_err(db_err)?;

    // Estimated row counts (cheap catalog estimates, like Railway) in one extra
    // query — keyed by "schema\0table". Negative/NULL estimates (e.g. a table
    // never ANALYZEd, or a view) are dropped so the UI shows no badge.
    let count_sql = match target.engine {
        Engine::Postgres => "SELECT n.nspname, c.relname, c.reltuples::bigint \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p')"
            .to_string(),
        Engine::Mysql => format!(
            "SELECT table_schema, table_name, table_rows \
             FROM information_schema.tables WHERE {filter}"
        ),
    };
    let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    if let Ok(cg) = db.fetch_text(&count_sql).await {
        for r in &cg {
            let s = r.first().cloned().flatten().unwrap_or_default();
            let t = r.get(1).cloned().flatten().unwrap_or_default();
            if let Some(n) = r
                .get(2)
                .cloned()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok())
            {
                if n >= 0 {
                    counts.insert(format!("{s}\u{0}{t}"), n);
                }
            }
        }
    }

    let mut schemas: Vec<serde_json::Value> = Vec::new();
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for row in &grid {
        let schema = row.first().cloned().flatten().unwrap_or_default();
        let name = row.get(1).cloned().flatten().unwrap_or_default();
        let table_type = row.get(2).cloned().flatten().unwrap_or_default();
        let kind = if table_type.contains("VIEW") {
            "view"
        } else {
            "table"
        };
        let rows_est = counts.get(&format!("{schema}\u{0}{name}")).copied();

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
            arr.push(serde_json::json!({ "name": name, "kind": kind, "rows": rows_est }));
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
    let target = resolve_target(&state, &id)?;
    let engine = target.engine;
    let database = q.database.unwrap_or_else(|| target.default_db.clone());
    let schema = q
        .schema
        .ok_or_else(|| AppError::BadRequest("schema is required".into()))?;
    let table = q
        .table
        .ok_or_else(|| AppError::BadRequest("table is required".into()))?;

    let mut db = connect(&state, &target, &database).await?;
    ensure_table_exists(&mut db, engine, &schema, &table).await?;

    // Primary-key columns — standard information_schema, identical on both.
    let pk_sql = format!(
        "SELECT kcu.column_name
         FROM information_schema.table_constraints tc
         JOIN information_schema.key_column_usage kcu
           ON tc.constraint_name = kcu.constraint_name
          AND tc.table_schema = kcu.table_schema
         WHERE tc.constraint_type = 'PRIMARY KEY'
           AND tc.table_schema = {} AND tc.table_name = {}",
        engine.quote_literal(&schema),
        engine.quote_literal(&table),
    );
    let pk_grid = db.fetch_text(&pk_sql).await.map_err(db_err)?;
    let pks: HashSet<String> = pk_grid
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .collect();

    let col_sql = format!(
        "SELECT column_name, data_type, is_nullable, column_default
         FROM information_schema.columns
         WHERE table_schema = {} AND table_name = {}
         ORDER BY ordinal_position",
        engine.quote_literal(&schema),
        engine.quote_literal(&table),
    );
    let col_grid = db.fetch_text(&col_sql).await.map_err(db_err)?;
    let columns: Vec<serde_json::Value> = col_grid
        .iter()
        .map(|r| {
            let name = r.first().cloned().flatten().unwrap_or_default();
            let is_pk = pks.contains(&name);
            serde_json::json!({
                "name": name,
                "type": r.get(1).cloned().flatten().unwrap_or_default(),
                "nullable": r.get(2).cloned().flatten().as_deref() == Some("YES"),
                "default": r.get(3).cloned().flatten(),
                "is_pk": is_pk,
            })
        })
        .collect();

    // Indexes are engine-specific.
    let idx_sql = match engine {
        Engine::Postgres => format!(
            "SELECT indexname, indexdef FROM pg_indexes
             WHERE schemaname = {} AND tablename = {} ORDER BY indexname",
            engine.quote_literal(&schema),
            engine.quote_literal(&table),
        ),
        Engine::Mysql => format!(
            "SELECT index_name, GROUP_CONCAT(column_name ORDER BY seq_in_index SEPARATOR ', ')
             FROM information_schema.statistics
             WHERE table_schema = {} AND table_name = {}
             GROUP BY index_name ORDER BY index_name",
            engine.quote_literal(&schema),
            engine.quote_literal(&table),
        ),
    };
    let idx_grid = db.fetch_text(&idx_sql).await.map_err(db_err)?;
    let indexes: Vec<serde_json::Value> = idx_grid
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.first().cloned().flatten().unwrap_or_default(),
                "def": r.get(1).cloned().flatten(),
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
pub async fn rows(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = resolve_target(&state, &id)?;
    let engine = target.engine;
    let database = q.database.unwrap_or_else(|| target.default_db.clone());
    let schema = q
        .schema
        .ok_or_else(|| AppError::BadRequest("schema is required".into()))?;
    let table = q
        .table
        .ok_or_else(|| AppError::BadRequest("table is required".into()))?;
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    let offset = q.offset.unwrap_or(0).max(0);

    let mut db = connect(&state, &target, &database).await?;
    ensure_table_exists(&mut db, engine, &schema, &table).await?;

    let col_sql = format!(
        "SELECT column_name FROM information_schema.columns
         WHERE table_schema = {} AND table_name = {} ORDER BY ordinal_position",
        engine.quote_literal(&schema),
        engine.quote_literal(&table),
    );
    let col_grid = db.fetch_text(&col_sql).await.map_err(db_err)?;
    let cols: Vec<String> = col_grid
        .iter()
        .filter_map(|r| r.first().cloned().flatten())
        .collect();

    if cols.is_empty() {
        return Ok(Json(serde_json::json!({
            "columns": [], "rows": [], "total": 0, "limit": limit, "offset": offset,
        })));
    }

    let select_list = cols
        .iter()
        .map(|c| engine.text_cast(&engine.quote_ident(c)))
        .collect::<Vec<_>>()
        .join(", ");
    let qualified = format!(
        "{}.{}",
        engine.quote_ident(&schema),
        engine.quote_ident(&table)
    );

    // ORDER BY: the sort column must be one of the table's real columns (never
    // trusted from the client), and the direction is a fixed allowlist.
    let order_by = match q.sort.as_deref() {
        Some(s) if cols.iter().any(|c| c == s) => {
            let dir = match q.dir.as_deref() {
                Some(d) if d.eq_ignore_ascii_case("desc") => "DESC",
                _ => "ASC",
            };
            format!(" ORDER BY {} {dir}", engine.quote_ident(s))
        }
        _ => String::new(),
    };

    let where_clause = build_filter(
        engine,
        &cols,
        q.filter_col.as_deref(),
        q.filter_op.as_deref(),
        q.filter_val.as_deref(),
    );

    // limit/offset are validated i64 — safe to inline.
    let sql = format!(
        "SELECT {select_list} FROM {qualified}{where_clause}{order_by} LIMIT {limit} OFFSET {offset}"
    );
    let grid = db.fetch_text(&sql).await.map_err(db_err)?;

    let count_sql = format!(
        "SELECT {} FROM {qualified}{where_clause}",
        engine.text_cast("count(*)")
    );
    let total: Option<i64> = db
        .fetch_text(&count_sql)
        .await
        .ok()
        .and_then(|g| g.into_iter().next())
        .and_then(|r| r.into_iter().next().flatten())
        .and_then(|s| s.parse::<i64>().ok());

    Ok(Json(serde_json::json!({
        "columns": cols,
        "rows": grid,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

// ── SQL runner ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct RunRequest {
    pub database: Option<String>,
    pub sql: String,
}

/// Outcome of a runner execution.
enum Outcome {
    Read {
        columns: Vec<String>,
        rows: Vec<Vec<Option<String>>>,
        truncated: bool,
    },
    Write {
        affected: i64,
    },
}

/// POST /api/v1/resources/{id}/db-browser/query — run an arbitrary statement.
/// `Editor+` because it can mutate. Each run is audited in `db_query_log`.
pub async fn run_query(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<RunRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let target = resolve_target(&state, &id)?;
    let database = body
        .database
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| target.default_db.clone());

    // Strip a single trailing `;` so the statement can be wrapped as a subquery.
    let sql = body.sql.trim().trim_end_matches(';').trim().to_string();
    if sql.is_empty() {
        return Err(AppError::BadRequest("SQL statement is empty".into()));
    }

    let read = is_read_stmt(&sql);
    let started = Instant::now();
    let result = if read {
        run_read(&state, &target, &database, &sql).await
    } else {
        run_write(&state, &target, &database, &sql).await
    };
    let duration_ms = started.elapsed().as_millis() as i64;

    let (status, row_count, error) = match &result {
        Ok(Outcome::Read { rows, .. }) => ("ok", Some(rows.len() as i64), None),
        Ok(Outcome::Write { affected }) => ("ok", Some(*affected), None),
        Err(e) => ("error", None, Some(e.to_string())),
    };
    log_query(
        &state,
        &id,
        &user,
        &database,
        &sql,
        if read { "read" } else { "write" },
        status,
        row_count,
        duration_ms,
        error.as_deref(),
    );

    let outcome = result?;
    let payload = match outcome {
        Outcome::Read {
            columns,
            rows,
            truncated,
        } => serde_json::json!({
            "kind": "read",
            "columns": columns,
            "rows": rows,
            "truncated": truncated,
            "duration_ms": duration_ms,
        }),
        Outcome::Write { affected } => serde_json::json!({
            "kind": "write",
            "rows_affected": affected,
            "duration_ms": duration_ms,
        }),
    };
    Ok(Json(payload))
}

/// Execute a rowset-returning statement. We learn its column names by preparing
/// it (no execution), then wrap it so every column is cast to text.
async fn run_read(
    state: &SharedState,
    target: &Target,
    database: &str,
    sql: &str,
) -> AppResult<Outcome> {
    let engine = target.engine;
    let mut db = connect(state, target, database).await?;

    let names = db.column_names(sql).await.map_err(db_err)?;

    // Statements that return no columns (e.g. `SET`) — run as an execute and
    // report affected rows.
    if names.is_empty() {
        let affected = db.exec(sql).await.map_err(db_err)? as i64;
        return Ok(Outcome::Write { affected });
    }

    if names.iter().any(|n| n.is_empty()) || has_duplicates(&names) {
        return Err(AppError::BadRequest(
            "Result has unnamed or duplicate columns. Add explicit column aliases to view it in the grid.".into(),
        ));
    }

    let select_list = names
        .iter()
        .map(|n| {
            let ident = engine.quote_ident(n);
            format!("{} AS {}", engine.text_cast(&ident), ident)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let wrapped = format!(
        "SELECT {select_list} FROM ({sql}) AS _pier_sub LIMIT {}",
        READ_CAP + 1
    );

    let mut data = db.fetch_text(&wrapped).await.map_err(db_err)?;
    let truncated = data.len() as i64 > READ_CAP;
    if truncated {
        data.truncate(READ_CAP as usize);
    }

    Ok(Outcome::Read {
        columns: names,
        rows: data,
        truncated,
    })
}

/// Execute a non-rowset statement (INSERT/UPDATE/DELETE/DDL/…); report affected
/// rows.
async fn run_write(
    state: &SharedState,
    target: &Target,
    database: &str,
    sql: &str,
) -> AppResult<Outcome> {
    let mut db = connect(state, target, database).await?;
    let affected = db.exec(sql).await.map_err(db_err)? as i64;
    Ok(Outcome::Write { affected })
}

/// Classify a statement as rowset-returning by its leading keyword (after
/// stripping leading comments). Conservative: anything not clearly a read is
/// executed as a write.
fn is_read_stmt(sql: &str) -> bool {
    let s = strip_leading_comments(sql);
    let first: String = s
        .chars()
        .take_while(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    matches!(
        first.as_str(),
        "select" | "with" | "values" | "table" | "show" | "explain" | "describe" | "desc"
    )
}

/// Drop leading `--` line comments and `/* … */` block comments + whitespace.
fn strip_leading_comments(sql: &str) -> &str {
    let mut s = sql.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            match rest.find('\n') {
                Some(nl) => s = rest[nl + 1..].trim_start(),
                None => return "",
            }
        } else if s.starts_with("/*") {
            match s.find("*/") {
                Some(end) => s = s[end + 2..].trim_start(),
                None => return "",
            }
        } else {
            break;
        }
    }
    s
}

fn has_duplicates(v: &[String]) -> bool {
    let mut seen = HashSet::new();
    v.iter().any(|x| !seen.insert(x.as_str()))
}

/// Best-effort audit row. Never fails the request — a logging error must not
/// mask a successful (or failed) query.
#[allow(clippy::too_many_arguments)]
pub(crate) fn log_query(
    state: &SharedState,
    service_id: &str,
    user: &AuthUser,
    database: &str,
    sql: &str,
    kind: &str,
    status: &str,
    row_count: Option<i64>,
    duration_ms: i64,
    error: Option<&str>,
) {
    if let Ok(db) = state.db.lock() {
        let log_id = uuid::Uuid::new_v4().to_string();
        let _ = db.execute(
            "INSERT INTO db_query_log
                (id, service_id, user_id, username, db_name, sql, kind, status, row_count, duration_ms, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                log_id,
                service_id,
                user.id,
                user.username,
                database,
                sql,
                kind,
                status,
                row_count,
                duration_ms,
                error,
            ],
        );
    }
}
