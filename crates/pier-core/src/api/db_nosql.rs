//! In-panel data browser for the non-SQL engines (Phase 3).
//!
//! - **Redis / Valkey** — native [`redis`] client over the loopback. Browse
//!   keys (SCAN + TYPE), inspect a key's value (type-aware), and run an
//!   arbitrary command.
//! - **MongoDB** — driven through `mongosh` via [`super::databases::exec_in_container`]
//!   (the same docker-exec path the existing DB management already uses), so we
//!   avoid the heavy native `mongodb` crate. Browse databases/collections,
//!   page through documents, and run an arbitrary script. Document JSON is
//!   produced with `EJSON.stringify` and fenced with markers so banner/warning
//!   noise on the exec stream can't corrupt the parse.
//!
//! Reads are `Viewer+`; the command/script runners are `Editor+` and audited in
//! `db_query_log` (reusing [`super::db_browser::log_query`]).

use std::collections::HashMap;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::time::{interval, Duration};

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

// ── shared service lookup ────────────────────────────────────────────────────

/// (catalog_id, service name, decrypted env map).
fn service_row(
    state: &SharedState,
    id: &str,
) -> AppResult<(String, String, HashMap<String, String>)> {
    let (catalog, name, env_json): (Option<String>, String, Option<String>) = {
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
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.db_nosql.resource_not_found",
                &[("v", id)],
            ))
        })?
    };
    let env: HashMap<String, String> =
        serde_json::from_str(&crate::crypto::decrypt_env_json(env_json.as_deref()))
            .unwrap_or_default();
    Ok((catalog.unwrap_or_default(), name, env))
}

/// Host port bound for `container_port`, falling back to the service's only port.
fn port_lookup(state: &SharedState, id: &str, container_port: i64) -> AppResult<u16> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let p: i64 = db
        .query_row(
            "SELECT host_port FROM port_allocations WHERE service_id = ?1 AND container_port = ?2 LIMIT 1",
            rusqlite::params![id, container_port],
            |r| r.get(0),
        )
        .or_else(|_| {
            db.query_row(
                "SELECT host_port FROM port_allocations WHERE service_id = ?1 ORDER BY host_port LIMIT 1",
                [id],
                |r| r.get(0),
            )
        })
        .map_err(|_| AppError::BadRequest(crate::i18n::te("errors.db_nosql.no_host_port")))?;
    Ok(p as u16)
}

// ── Redis ────────────────────────────────────────────────────────────────────

struct RedisTarget {
    container: String,
    host_port: u16,
    password: String,
}

fn redis_target(state: &SharedState, id: &str) -> AppResult<RedisTarget> {
    let (catalog, name, env) = service_row(state, id)?;
    if !matches!(catalog.as_str(), "redis" | "valkey") {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.db_nosql.not_redis_service",
        )));
    }
    // Valkey stores its password in VALKEY_PASSWORD (its template runs
    // `valkey-server --requirepass {{VALKEY_PASSWORD}}`); fall back to it so the
    // whole Redis browser (keys/value/command/keyspace/monitor) authenticates.
    let password = env
        .get("REDIS_PASSWORD")
        .or_else(|| env.get("VALKEY_PASSWORD"))
        .cloned()
        .unwrap_or_default();
    let host_port = port_lookup(state, id, 6379)?;
    Ok(RedisTarget {
        container: format!("pier-{}", name.to_lowercase().replace(' ', "-")),
        host_port,
        password,
    })
}

fn redis_err(e: redis::RedisError) -> AppError {
    AppError::BadRequest(crate::i18n::te_args(
        "errors.db_nosql.redis_error",
        &[("v", &e.to_string())],
    ))
}

async fn redis_conn(
    state: &SharedState,
    target: &RedisTarget,
    db: i64,
) -> AppResult<redis::aio::MultiplexedConnection> {
    let db = db.clamp(0, 15);
    // Prefer the container's pier-net IP + internal 6379 (works for unpublished
    // services); fall back to the host loopback port.
    let (host, port) =
        match super::db_browser::container_host(&state.docker, &target.container).await {
            Some(ip) => (ip, 6379u16),
            None => ("127.0.0.1".to_string(), target.host_port),
        };
    let url = if target.password.is_empty() {
        format!("redis://{host}:{port}/{db}")
    } else {
        format!(
            "redis://:{}@{host}:{port}/{db}",
            urlencoding::encode(&target.password)
        )
    };
    let client = redis::Client::open(url).map_err(|e| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.db_nosql.redis_client",
            &[("v", &e.to_string())],
        ))
    })?;
    client
        .get_multiplexed_async_connection()
        .await
        .map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.db_nosql.redis_connect_failed",
                &[("v", &e.to_string())],
            ))
        })
}

/// Convert a Redis reply into JSON for display.
fn redis_value_to_json(v: &redis::Value) -> serde_json::Value {
    match v {
        redis::Value::Nil => serde_json::Value::Null,
        redis::Value::Int(i) => serde_json::json!(i),
        redis::Value::BulkString(b) => serde_json::json!(String::from_utf8_lossy(b)),
        redis::Value::SimpleString(s) => serde_json::json!(s),
        redis::Value::Okay => serde_json::json!("OK"),
        redis::Value::Array(a) | redis::Value::Set(a) => {
            serde_json::Value::Array(a.iter().map(redis_value_to_json).collect())
        }
        redis::Value::Map(m) => {
            let mut o = serde_json::Map::new();
            for (k, val) in m {
                o.insert(redis_value_to_plain(k), redis_value_to_json(val));
            }
            serde_json::Value::Object(o)
        }
        redis::Value::Double(d) => serde_json::json!(d),
        redis::Value::Boolean(b) => serde_json::json!(b),
        other => serde_json::json!(format!("{other:?}")),
    }
}

fn redis_value_to_plain(v: &redis::Value) -> String {
    match v {
        redis::Value::BulkString(b) => String::from_utf8_lossy(b).into_owned(),
        redis::Value::SimpleString(s) => s.clone(),
        redis::Value::Int(i) => i.to_string(),
        other => format!("{other:?}"),
    }
}

#[derive(Deserialize)]
pub struct RedisKeysQuery {
    pub pattern: Option<String>,
    pub cursor: Option<u64>,
    pub db: Option<i64>,
}

/// GET /api/v1/resources/{id}/db-browser/redis/keys
pub async fn redis_keys(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<RedisKeysQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = redis_target(&state, &id)?;
    let dbidx = q.db.unwrap_or(0);
    let mut con = redis_conn(&state, &target, dbidx).await?;

    let pattern = q
        .pattern
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "*".into());

    // A single SCAN may legally return zero keys with a non-zero cursor (the
    // batch covered empty hash-table slots). Loop from the incoming cursor until
    // we've gathered a page worth of keys or the cursor wraps to 0 — otherwise a
    // populated DB looks empty and the user has to keep clicking "Load more".
    let mut cursor = q.cursor.unwrap_or(0);
    let mut keys: Vec<String> = Vec::new();
    for _ in 0..50 {
        let (next, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query_async(&mut con)
            .await
            .map_err(redis_err)?;
        cursor = next;
        keys.extend(batch);
        if cursor == 0 || keys.len() >= 200 {
            break;
        }
    }
    let next = cursor;

    let mut out = Vec::with_capacity(keys.len());
    for k in &keys {
        let t: String = redis::cmd("TYPE")
            .arg(k)
            .query_async(&mut con)
            .await
            .unwrap_or_else(|_| "unknown".into());
        out.push(serde_json::json!({ "key": k, "type": t }));
    }

    Ok(Json(
        serde_json::json!({ "keys": out, "cursor": next, "db": dbidx }),
    ))
}

/// GET /api/v1/resources/{id}/db-browser/redis/keyspace — key count per logical
/// DB, so the UI can show where the data lives (and auto-pick a non-empty DB).
pub async fn redis_keyspace(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = redis_target(&state, &id)?;
    let mut con = redis_conn(&state, &target, 0).await?;

    // `INFO keyspace` returns lines like `db0:keys=1204,expires=3,avg_ttl=0`.
    let info: String = redis::cmd("INFO")
        .arg("keyspace")
        .query_async(&mut con)
        .await
        .map_err(redis_err)?;

    let mut counts = serde_json::Map::new();
    for line in info.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("db") {
            if let Some((idx, fields)) = rest.split_once(':') {
                let keys = fields
                    .split(',')
                    .find_map(|kv| kv.strip_prefix("keys="))
                    .and_then(|n| n.parse::<i64>().ok())
                    .unwrap_or(0);
                counts.insert(idx.to_string(), serde_json::json!(keys));
            }
        }
    }

    Ok(Json(serde_json::json!({ "keyspace": counts })))
}

#[derive(Deserialize)]
pub struct RedisValueQuery {
    pub key: String,
    pub db: Option<i64>,
}

/// GET /api/v1/resources/{id}/db-browser/redis/value
pub async fn redis_value(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<RedisValueQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let target = redis_target(&state, &id)?;
    let mut con = redis_conn(&state, &target, q.db.unwrap_or(0)).await?;

    let key = q.key;
    let t: String = redis::cmd("TYPE")
        .arg(&key)
        .query_async(&mut con)
        .await
        .map_err(redis_err)?;

    let raw: redis::Value = match t.as_str() {
        "string" => redis::cmd("GET").arg(&key).query_async(&mut con).await,
        "list" => {
            redis::cmd("LRANGE")
                .arg(&key)
                .arg(0)
                .arg(500)
                .query_async(&mut con)
                .await
        }
        "set" => redis::cmd("SMEMBERS").arg(&key).query_async(&mut con).await,
        "zset" => {
            redis::cmd("ZRANGE")
                .arg(&key)
                .arg(0)
                .arg(500)
                .arg("WITHSCORES")
                .query_async(&mut con)
                .await
        }
        "hash" => redis::cmd("HGETALL").arg(&key).query_async(&mut con).await,
        "stream" => {
            redis::cmd("XRANGE")
                .arg(&key)
                .arg("-")
                .arg("+")
                .arg("COUNT")
                .arg(100)
                .query_async(&mut con)
                .await
        }
        _ => Ok(redis::Value::Nil),
    }
    .map_err(redis_err)?;

    let ttl: i64 = redis::cmd("TTL")
        .arg(&key)
        .query_async(&mut con)
        .await
        .unwrap_or(-1);

    Ok(Json(serde_json::json!({
        "key": key,
        "type": t,
        "ttl": ttl,
        "value": redis_value_to_json(&raw),
    })))
}

#[derive(Deserialize)]
pub struct RedisCommandRequest {
    pub command: String,
    pub db: Option<i64>,
}

/// POST /api/v1/resources/{id}/db-browser/redis/command — run a raw command.
pub async fn redis_command(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<RedisCommandRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let target = redis_target(&state, &id)?;
    let dbidx = body.db.unwrap_or(0);
    let args = tokenize(&body.command);
    if args.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.db_nosql.command_empty",
        )));
    }

    let started = Instant::now();
    let result = run_redis_command(&state, &target, dbidx, &args).await;
    let duration_ms = started.elapsed().as_millis() as i64;

    let (status, error) = match &result {
        Ok(_) => ("ok", None),
        Err(e) => ("error", Some(e.to_string())),
    };
    super::db_browser::log_query(
        &state,
        &id,
        &user,
        &format!("db{dbidx}"),
        &body.command,
        "redis",
        status,
        None,
        duration_ms,
        error.as_deref(),
    );

    let value = result?;
    Ok(Json(
        serde_json::json!({ "result": value, "duration_ms": duration_ms }),
    ))
}

async fn run_redis_command(
    state: &SharedState,
    target: &RedisTarget,
    dbidx: i64,
    args: &[String],
) -> AppResult<serde_json::Value> {
    let mut con = redis_conn(state, target, dbidx).await?;
    let mut cmd = redis::cmd(args[0].as_str());
    for a in &args[1..] {
        cmd.arg(a.as_str());
    }
    let v: redis::Value = cmd.query_async(&mut con).await.map_err(redis_err)?;
    Ok(redis_value_to_json(&v))
}

/// Split a command line into tokens, honouring double quotes.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for ch in s.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ── Redis live MONITOR (WebSocket) ───────────────────────────────────────────

/// GET /api/v1/resources/{id}/db-browser/redis/monitor/ws — live `MONITOR` feed.
///
/// `MONITOR` streams every command the server executes (with the DB index and
/// key), so an operator can see ephemeral/TTL key activity that a `SCAN`
/// snapshot can't catch and that Redis never writes to its log. It's `Editor+`
/// (it exposes all commands and their data) and only runs while the socket is
/// open — closing it drops the dedicated connection and removes the MONITOR.
pub async fn redis_monitor_ws(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> AppResult<axum::response::Response> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let target = redis_target(&state, &id)?;

    // Same host/port resolution as redis_conn (DB index is irrelevant for
    // MONITOR — it's server-wide).
    let (host, port) =
        match super::db_browser::container_host(&state.docker, &target.container).await {
            Some(ip) => (ip, 6379u16),
            None => ("127.0.0.1".to_string(), target.host_port),
        };
    let url = if target.password.is_empty() {
        format!("redis://{host}:{port}")
    } else {
        format!(
            "redis://:{}@{host}:{port}",
            urlencoding::encode(&target.password)
        )
    };

    // Audit the session start (best-effort, like the query log).
    super::db_browser::log_query(
        &state,
        &id,
        &user,
        "-",
        "MONITOR",
        "redis-monitor",
        "ok",
        None,
        0,
        None,
    );

    Ok(ws.on_upgrade(move |socket| async move {
        stream_redis_monitor(url, socket).await;
    }))
}

async fn stream_redis_monitor(url: String, mut socket: WebSocket) {
    let client = match redis::Client::open(url) {
        Ok(c) => c,
        Err(e) => {
            let _ = socket
                .send(Message::Text(format!("[pier] redis client: {e}").into()))
                .await;
            return;
        }
    };
    let monitor = match client.get_async_monitor().await {
        Ok(m) => m,
        Err(e) => {
            let _ = socket
                .send(Message::Text(
                    format!("[pier] could not start MONITOR: {e}").into(),
                ))
                .await;
            return;
        }
    };
    let mut stream = monitor.into_on_message::<String>();
    let mut ping_tick = interval(Duration::from_secs(30));
    ping_tick.tick().await; // skip immediate fire

    loop {
        tokio::select! {
            line = stream.next() => {
                match line {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break, // monitor connection closed
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
    // Dropping `stream` (and its connection) ends the MONITOR on the server.
}

// ── MongoDB (via mongosh) ────────────────────────────────────────────────────

struct MongoTarget {
    container: String,
    user: String,
    password: String,
}

fn mongo_target(state: &SharedState, id: &str) -> AppResult<MongoTarget> {
    let (catalog, name, env) = service_row(state, id)?;
    if catalog != "mongodb" {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.db_nosql.not_mongodb_service",
        )));
    }
    let user = env
        .get("MONGO_INITDB_ROOT_USERNAME")
        .cloned()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "root".into());
    let password = env
        .get("MONGO_INITDB_ROOT_PASSWORD")
        .cloned()
        .unwrap_or_default();
    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    Ok(MongoTarget {
        container,
        user,
        password,
    })
}

/// Escape a string as a JS double-quoted literal for embedding in a mongosh eval.
fn js_string(s: &str) -> String {
    let e = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{e}\"")
}

/// Run a mongosh `--eval` script in the service container, returning stdout+stderr.
async fn mongosh_eval(state: &SharedState, t: &MongoTarget, eval: &str) -> AppResult<String> {
    let cmd: Vec<&str> = vec![
        "mongosh",
        "--quiet",
        "--username",
        &t.user,
        "--password",
        &t.password,
        "--authenticationDatabase",
        "admin",
        "--eval",
        eval,
    ];
    super::databases::exec_in_container(&state.docker, &t.container, &cmd).await
}

fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let i = s.find(start)? + start.len();
    let j = s[i..].find(end)? + i;
    Some(&s[i..j])
}

/// Evaluate `expr` (a JS expression) and parse its `EJSON.stringify` output.
/// The result is fenced with markers so banner/warning noise can't corrupt the
/// parse.
async fn mongo_eval_json(
    state: &SharedState,
    t: &MongoTarget,
    expr: &str,
) -> AppResult<serde_json::Value> {
    let script = format!("print('<<PIER<<' + EJSON.stringify({expr}) + '>>PIER>>')");
    let out = mongosh_eval(state, t, &script).await?;
    let json_str = between(&out, "<<PIER<<", ">>PIER>>").ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.db_nosql.unexpected_mongosh_output",
            &[("v", out.trim())],
        ))
    })?;
    serde_json::from_str(json_str).map_err(|e| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.db_nosql.parse_mongo_output_failed",
            &[("v", &e.to_string())],
        ))
    })
}

/// GET /api/v1/resources/{id}/db-browser/mongo/databases
pub async fn mongo_databases(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let t = mongo_target(&state, &id)?;
    let dbs = mongo_eval_json(
        &state,
        &t,
        "db.adminCommand({listDatabases:1}).databases.map(d=>d.name)",
    )
    .await?;
    Ok(Json(serde_json::json!({ "databases": dbs })))
}

#[derive(Deserialize)]
pub struct MongoCollectionsQuery {
    pub database: String,
}

/// GET /api/v1/resources/{id}/db-browser/mongo/collections
pub async fn mongo_collections(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<MongoCollectionsQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let t = mongo_target(&state, &id)?;
    let expr = format!(
        "db.getSiblingDB({}).getCollectionNames()",
        js_string(&q.database)
    );
    let collections = mongo_eval_json(&state, &t, &expr).await?;
    Ok(Json(serde_json::json!({ "collections": collections })))
}

#[derive(Deserialize)]
pub struct MongoDocsQuery {
    pub database: String,
    pub collection: String,
    pub limit: Option<i64>,
    pub skip: Option<i64>,
}

/// GET /api/v1/resources/{id}/db-browser/mongo/documents
pub async fn mongo_documents(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Query(q): Query<MongoDocsQuery>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let t = mongo_target(&state, &id)?;
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let skip = q.skip.unwrap_or(0).max(0);
    let dbq = js_string(&q.database);
    let collq = js_string(&q.collection);

    let expr = format!(
        "db.getSiblingDB({dbq}).getCollection({collq}).find().skip({skip}).limit({limit}).toArray()"
    );
    let documents = mongo_eval_json(&state, &t, &expr).await?;

    let count_expr = format!("db.getSiblingDB({dbq}).getCollection({collq}).countDocuments({{}})");
    let total = mongo_eval_json(&state, &t, &count_expr).await.ok();

    Ok(Json(serde_json::json!({
        "documents": documents,
        "total": total,
        "limit": limit,
        "skip": skip,
    })))
}

#[derive(Deserialize)]
pub struct MongoQueryRequest {
    pub database: String,
    pub script: String,
}

/// POST /api/v1/resources/{id}/db-browser/mongo/query — run a raw mongosh script.
pub async fn mongo_query(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<MongoQueryRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let t = mongo_target(&state, &id)?;
    let script = body.script.trim().to_string();
    if script.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.db_nosql.script_empty",
        )));
    }

    let eval = format!(
        "db = db.getSiblingDB({}); {}",
        js_string(&body.database),
        script
    );
    let started = Instant::now();
    let result = mongosh_eval(&state, &t, &eval).await;
    let duration_ms = started.elapsed().as_millis() as i64;

    let (status, error) = match &result {
        Ok(_) => ("ok", None),
        Err(e) => ("error", Some(e.to_string())),
    };
    super::db_browser::log_query(
        &state,
        &id,
        &user,
        &body.database,
        &script,
        "mongo",
        status,
        None,
        duration_ms,
        error.as_deref(),
    );

    let output = result?;
    Ok(Json(serde_json::json!({
        "output": output.trim(),
        "duration_ms": duration_ms,
    })))
}
