use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

use super::resources::purge_backup_blobs;
use super::security::{self, DeleteRequest};

/// Fetch a service's decrypted env as a map. Needed for mongosh root auth.
fn fetch_env_vars(state: &SharedState, service_id: &str) -> AppResult<HashMap<String, String>> {
    let env_json: Option<String> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT env_json FROM services WHERE id = ?1",
            [service_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.databases.resource_not_found",
                &[("id", service_id)],
            ))
        })?
    };
    let decrypted = crate::crypto::decrypt_env_json(env_json.as_deref());
    Ok(serde_json::from_str(&decrypted).unwrap_or_default())
}

/// Escape a value as a JS double-quoted string literal. Used when embedding
/// user-supplied passwords in mongosh `--eval` scripts.
fn js_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// GET /api/v1/resources/{id}/databases — list databases in a PostgreSQL/MySQL container.
/// Editor+ — DB metadata leaks usernames and we treat that as semi-sensitive.
pub async fn list_databases(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let (catalog_id, name) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, name FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.databases.resource_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    let output = match catalog.as_str() {
        "postgresql" | "postgis" | "timescaledb" => {
            exec_in_container(
                &state.docker,
                &container,
                &[
                    "psql",
                    "-U",
                    "postgres",
                    "-t",
                    "-A",
                    "-F",
                    "|",
                    "-c",
                    "SELECT d.datname, r.rolname, pg_size_pretty(pg_database_size(d.datname)), r.rolcreaterole, r.rolcreatedb, r.rolsuper FROM pg_database d JOIN pg_roles r ON d.datdba = r.oid WHERE d.datistemplate = false ORDER BY d.datname",
                ],
            )
            .await?
        }
        "mysql" | "mariadb" => {
            exec_in_container(
                &state.docker,
                &container,
                &[
                    "mysql",
                    "-u",
                    "root",
                    "-e",
                    "SELECT SCHEMA_NAME, '—', CONCAT(ROUND(SUM(data_length + index_length) / 1024 / 1024, 1), ' MB') FROM information_schema.SCHEMATA LEFT JOIN information_schema.TABLES ON SCHEMA_NAME = TABLE_SCHEMA GROUP BY SCHEMA_NAME",
                ],
            )
            .await?
        }
        "mongodb" => {
            // MongoDB has no per-DB "owner" concept. We list only DBs created
            // through this UI (tracked in database_credentials); system DBs
            // (admin / local / config) and lazily-created ones are omitted.
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            let mut stmt = db.prepare(
                "SELECT db_name, username, password FROM database_credentials
                 WHERE service_id = ?1 ORDER BY db_name",
            )?;
            let rows: Vec<serde_json::Value> = stmt
                .query_map([&id], |row| {
                    Ok(serde_json::json!({
                        "name": row.get::<_, String>(0)?,
                        "owner": row.get::<_, String>(1)?,
                        "size": "—",
                        "stored_password": row.get::<_, String>(2)?,
                    }))
                })?
                .filter_map(|r| r.ok())
                .collect();
            return Ok(Json(rows));
        }
        _ => {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.databases.management_unsupported_engine",
            )));
        }
    };

    // Parse output into structured data
    // Load stored credentials
    let creds: std::collections::HashMap<String, (String, String)> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT db_name, username, password FROM database_credentials WHERE service_id = ?1",
        )?;
        let rows = stmt
            .query_map([&id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    (row.get::<_, String>(1)?, row.get::<_, String>(2)?),
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        rows
    };

    let databases: Vec<serde_json::Value> = output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            let db_name = parts.first().map(|s| s.trim()).unwrap_or("");
            let cred = creds.get(db_name);
            serde_json::json!({
                "name": db_name,
                "owner": parts.get(1).map(|s| s.trim()).unwrap_or(""),
                "size": parts.get(2).map(|s| s.trim()).unwrap_or("0"),
                "stored_password": cred.map(|(_, p)| p.as_str()).unwrap_or(""),
                // PostgreSQL only — read live from pg_roles so the toggles
                // never disagree with the database. `null` for other engines.
                "createrole": pg_bool(parts.get(3)),
                "createdb": pg_bool(parts.get(4)),
                "superuser": pg_bool(parts.get(5)),
            })
        })
        .filter(|d| {
            let name = d["name"].as_str().unwrap_or("");
            !name.is_empty()
                && name != "template0"
                && name != "template1"
                && name != "template_postgis"
        })
        .collect();

    Ok(Json(databases))
}

/// POST /api/v1/resources/{id}/databases — create database + user.
pub async fn create_database(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<CreateDatabaseRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let db_name = body.database.trim();
    let username = body.username.trim();
    let password = &body.password;

    if db_name.is_empty() || username.is_empty() || password.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.databases.credentials_required",
        )));
    }

    // Validate names (alphanumeric + underscore only)
    let valid = |s: &str| s.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !valid(db_name) || !valid(username) {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.databases.invalid_name_chars",
        )));
    }

    let (catalog_id, name) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, name FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.databases.resource_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    match catalog.as_str() {
        "postgresql" | "postgis" | "timescaledb" => {
            // Validate requested PostGIS extensions before touching the DB.
            // Silently drop unknown names (the UI shouldn't ever send any),
            // and ignore the field entirely on plain `postgresql`.
            let extensions: Vec<String> = if catalog == "postgis" {
                body.extensions
                    .iter()
                    .filter(|e| POSTGIS_EXTENSIONS.iter().any(|allowed| allowed == e))
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            };

            // An existing role gets the password from the form instead of a
            // silently failing CREATE — otherwise the panel stores a password
            // the database never got.
            let exists_sql = format!(
                "SELECT 1 FROM pg_roles WHERE rolname = {}",
                pg_literal(&username.to_lowercase())
            );
            let role_exists = exec_checked(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-t", "-A", "-c", &exists_sql],
            )
            .await?
            .trim()
                == "1";
            let role_sql = pg_role_sql(
                role_exists,
                username,
                password,
                body.createrole,
                body.createdb,
            );

            // Each command must run separately — CREATE DATABASE cannot run inside a transaction
            exec_checked(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &role_sql],
            )
            .await?;

            let create_db = format!("CREATE DATABASE {db_name} OWNER {username}");
            exec_checked(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &create_db],
            )
            .await?;

            let grant = format!("GRANT ALL PRIVILEGES ON DATABASE {db_name} TO {username}");
            exec_checked(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &grant],
            )
            .await?;

            if !extensions.is_empty() {
                install_postgis_extensions(&state.docker, &container, db_name, &extensions).await?;
            }
        }
        "mysql" | "mariadb" => {
            let sql = format!(
                "CREATE DATABASE IF NOT EXISTS {db_name}; CREATE USER IF NOT EXISTS '{username}'@'%' IDENTIFIED BY '{password}'; GRANT ALL PRIVILEGES ON {db_name}.* TO '{username}'@'%'; FLUSH PRIVILEGES;"
            );
            exec_checked(
                &state.docker,
                &container,
                &["mysql", "-u", "root", "-e", &sql],
            )
            .await?;
        }
        "mongodb" => {
            let env = fetch_env_vars(&state, &id)?;
            let root_user = env
                .get("MONGO_INITDB_ROOT_USERNAME")
                .cloned()
                .unwrap_or_else(|| "root".into());
            let root_pass = env
                .get("MONGO_INITDB_ROOT_PASSWORD")
                .cloned()
                .unwrap_or_default();
            // db_name/username are already validated to [A-Za-z0-9_]; password is
            // embedded as a JS string literal (quotes/backslashes escaped).
            let pwd_js = js_string(password);
            let eval = format!(
                "db = db.getSiblingDB('{db_name}'); \
                 db.createUser({{user:'{username}', pwd:{pwd_js}, roles:[{{role:'readWrite', db:'{db_name}'}}]}}); \
                 db.pier_init.insertOne({{_init:1}}); \
                 db.pier_init.drop();"
            );
            exec_checked(
                &state.docker,
                &container,
                &[
                    "mongosh",
                    "--quiet",
                    "--username",
                    &root_user,
                    "--password",
                    &root_pass,
                    "--authenticationDatabase",
                    "admin",
                    "--eval",
                    &eval,
                ],
            )
            .await?;
        }
        _ => {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.databases.unsupported_type",
            )));
        }
    }

    // Store credentials. A re-used role just had its password changed, so
    // every database it owns gets the new one — not only this row.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        store_credentials(&db, &id, db_name, username, password, catalog != "mongodb")?;
    }

    tracing::info!("Created database {db_name} with user {username} in {container}");

    Ok(Json(serde_json::json!({
        "ok": true,
        "database": db_name,
        "username": username,
    })))
}

/// DELETE /api/v1/resources/{id}/databases/{dbname} — drop database + user.
///
/// Optional `?delete_backups=true` removes S3 blobs scoped exactly to this
/// `(service_id, database_name)` pair. Cluster-wide backups
/// (`database_name IS NULL`) are intentionally left untouched: they hold
/// dumps of all DBs in the service and dropping them on a single-DB delete
/// would discard data belonging to siblings.
pub async fn delete_database(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dbname)): Path<(String, String)>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
    Json(body): Json<DeleteRequest>,
) -> AppResult<impl IntoResponse> {
    // Dropping a DB is destructive — require project Admin.
    enforce_resource_role(&state, &user, &id, ProjectRole::Admin)?;
    let delete_backups = params
        .get("delete_backups")
        .map(|v| v == "true")
        .unwrap_or(false);

    let (catalog_id, name) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, name FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.databases.resource_not_found",
                &[("id", &id)],
            ))
        })?
    };

    if matches!(
        dbname.as_str(),
        "postgres"
            | "mysql"
            | "information_schema"
            | "admin"
            | "local"
            | "config"
            | "template_postgis"
    ) {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.databases.cannot_delete_system",
        )));
    }

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        security::verify_delete_password(&db, &user.id, body.password.as_deref())?;
    }

    // Drop S3 blobs scoped to this DB only. We capture (storage_id, key)
    // tuples and the matching backup row IDs up front because the DROP
    // DATABASE call doesn't touch SQLite — we'll need to delete the rows
    // explicitly after the blobs are gone.
    let (backup_blobs, backup_row_ids): (Vec<(String, String)>, Vec<String>) = if delete_backups {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT id, s3_storage_id, s3_key FROM backups
             WHERE service_id = ?1 AND database_name = ?2",
        )?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map(rusqlite::params![id, dbname], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();
        let ids = rows.iter().map(|r| r.0.clone()).collect();
        let blobs = rows.into_iter().map(|(_, s, k)| (s, k)).collect();
        (blobs, ids)
    } else {
        (Vec::new(), Vec::new())
    };

    if delete_backups && !backup_blobs.is_empty() {
        purge_backup_blobs(&state, &backup_blobs).await;
    }

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    match catalog.as_str() {
        "postgresql" | "postgis" | "timescaledb" => {
            // Get owner before dropping
            let owner_output = exec_in_container(
                &state.docker,
                &container,
                &[
                    "psql",
                    "-U",
                    "postgres",
                    "-t",
                    "-A",
                    "-c",
                    &format!(
                        "SELECT r.rolname FROM pg_database d JOIN pg_roles r ON d.datdba = r.oid WHERE d.datname = '{dbname}'"
                    ),
                ],
            )
            .await?;
            let owner = owner_output.trim().to_string();

            // Drop database
            exec_in_container(
                &state.docker,
                &container,
                &[
                    "psql",
                    "-U",
                    "postgres",
                    "-c",
                    &format!("DROP DATABASE IF EXISTS {dbname}"),
                ],
            )
            .await?;

            // Drop owner user if not postgres
            if !owner.is_empty() && owner != "postgres" {
                let _ = exec_in_container(
                    &state.docker,
                    &container,
                    &[
                        "psql",
                        "-U",
                        "postgres",
                        "-c",
                        &format!("DROP USER IF EXISTS {owner}"),
                    ],
                )
                .await;
            }
        }
        "mysql" | "mariadb" => {
            exec_in_container(
                &state.docker,
                &container,
                &[
                    "mysql",
                    "-u",
                    "root",
                    "-e",
                    &format!("DROP DATABASE IF EXISTS {dbname}"),
                ],
            )
            .await?;
        }
        "mongodb" => {
            let env = fetch_env_vars(&state, &id)?;
            let root_user = env
                .get("MONGO_INITDB_ROOT_USERNAME")
                .cloned()
                .unwrap_or_else(|| "root".into());
            let root_pass = env
                .get("MONGO_INITDB_ROOT_PASSWORD")
                .cloned()
                .unwrap_or_default();
            let eval = format!(
                "db = db.getSiblingDB('{dbname}'); \
                 db.getUsers().forEach(u => db.dropUser(u.user)); \
                 db.dropDatabase();"
            );
            exec_in_container(
                &state.docker,
                &container,
                &[
                    "mongosh",
                    "--quiet",
                    "--username",
                    &root_user,
                    "--password",
                    &root_pass,
                    "--authenticationDatabase",
                    "admin",
                    "--eval",
                    &eval,
                ],
            )
            .await?;
        }
        _ => {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.databases.unsupported_type",
            )));
        }
    }

    // Remove stored credentials, DB-scoped backup rows, and DB-scoped
    // schedules. Cluster-wide schedules/backups (database_name IS NULL)
    // intentionally untouched — see fn-doc.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "DELETE FROM database_credentials WHERE service_id = ?1 AND db_name = ?2",
            rusqlite::params![id, dbname],
        );
        let _ = db.execute(
            "DELETE FROM backup_schedules WHERE service_id = ?1 AND database_name = ?2",
            rusqlite::params![id, dbname],
        );
        if delete_backups && !backup_row_ids.is_empty() {
            let placeholders: Vec<&str> = (0..backup_row_ids.len()).map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM backups WHERE id IN ({})",
                placeholders.join(",")
            );
            let params_vec: Vec<&dyn rusqlite::ToSql> = backup_row_ids
                .iter()
                .map(|s| s as &dyn rusqlite::ToSql)
                .collect();
            let _ = db.execute(&sql, params_vec.as_slice());
        }
    }

    tracing::info!("Deleted database {dbname} from {container}");
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub password: String,
}

/// PUT /api/v1/resources/{id}/databases/{dbname}/password — change database user password.
pub async fn change_password(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dbname)): Path<(String, String)>,
    Json(body): Json<ChangePasswordRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let password = body.password.trim();
    if password.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.databases.password_required",
        )));
    }

    let (catalog_id, name) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, name FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.databases.resource_not_found",
                &[("id", &id)],
            ))
        })?
    };

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    // Get username: from stored credentials, or query PostgreSQL for DB owner
    let username: String = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT username FROM database_credentials WHERE service_id = ?1 AND db_name = ?2",
            rusqlite::params![id, dbname],
            |row| row.get(0),
        )
        .ok()
    }
    .unwrap_or_else(|| {
        // Fallback: query DB owner from PostgreSQL
        String::new()
    });

    // If no stored username, get it from the database engine
    let username = if username.is_empty() {
        match catalog.as_str() {
            "postgresql" | "postgis" | "timescaledb" => {
                let output = exec_in_container(&state.docker, &container, &[
                    "psql", "-U", "postgres", "-t", "-A", "-c",
                    &format!("SELECT r.rolname FROM pg_database d JOIN pg_roles r ON d.datdba = r.oid WHERE d.datname = '{dbname}'"),
                ]).await?;
                output.trim().to_string()
            }
            _ => dbname.clone(),
        }
    } else {
        username
    };

    if username.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.databases.owner_not_found",
            &[("name", &dbname)],
        )));
    }

    match catalog.as_str() {
        "postgresql" | "postgis" | "timescaledb" => {
            let sql = format!(
                "ALTER USER {username} WITH PASSWORD {}",
                pg_literal(password)
            );
            exec_checked(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &sql],
            )
            .await?;
        }
        "mysql" | "mariadb" => {
            let sql = format!(
                "ALTER USER '{username}'@'%' IDENTIFIED BY '{password}'; FLUSH PRIVILEGES;"
            );
            exec_checked(
                &state.docker,
                &container,
                &["mysql", "-u", "root", "-e", &sql],
            )
            .await?;
        }
        "mongodb" => {
            let env = fetch_env_vars(&state, &id)?;
            let root_user = env
                .get("MONGO_INITDB_ROOT_USERNAME")
                .cloned()
                .unwrap_or_else(|| "root".into());
            let root_pass = env
                .get("MONGO_INITDB_ROOT_PASSWORD")
                .cloned()
                .unwrap_or_default();
            let pwd_js = js_string(password);
            let eval = format!(
                "db = db.getSiblingDB('{dbname}'); \
                 db.changeUserPassword('{username}', {pwd_js});"
            );
            exec_checked(
                &state.docker,
                &container,
                &[
                    "mongosh",
                    "--quiet",
                    "--username",
                    &root_user,
                    "--password",
                    &root_pass,
                    "--authenticationDatabase",
                    "admin",
                    "--eval",
                    &eval,
                ],
            )
            .await?;
        }
        _ => {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.databases.unsupported_type",
            )))
        }
    }

    // Upsert stored credentials
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        store_credentials(&db, &id, &dbname, &username, password, catalog != "mongodb")?;
    }

    tracing::info!("Changed password for user {username} (db: {dbname}) in {container}");
    Ok(Json(serde_json::json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct PrivilegesRequest {
    #[serde(default)]
    pub createrole: bool,
    #[serde(default)]
    pub createdb: bool,
}

/// PUT /api/v1/resources/{id}/databases/{dbname}/privileges — set the
/// owner role's `CREATEROLE` / `CREATEDB`. PostgreSQL family only; never
/// touches a superuser (`postgres`), and never grants `SUPERUSER`.
///
/// The privileges belong to the role, so they apply to every database the
/// role owns.
pub async fn set_privileges(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dbname)): Path<(String, String)>,
    Json(body): Json<PrivilegesRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    if dbname.is_empty() || !dbname.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.databases.invalid_name_chars",
        )));
    }

    let (catalog_id, name) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT catalog_id, name FROM services WHERE id = ?1",
            [&id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|_| {
            AppError::NotFound(crate::i18n::te_args(
                "errors.databases.resource_not_found",
                &[("id", &id)],
            ))
        })?
    };
    if !matches!(
        catalog_id.as_deref(),
        Some("postgresql" | "postgis" | "timescaledb")
    ) {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.databases.privileges_unsupported_engine",
        )));
    }
    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));

    let owner_sql = format!(
        "SELECT r.rolname, r.rolsuper FROM pg_database d JOIN pg_roles r ON d.datdba = r.oid WHERE d.datname = {}",
        pg_literal(&dbname)
    );
    let out = exec_checked(
        &state.docker,
        &container,
        &[
            "psql", "-U", "postgres", "-t", "-A", "-F", "|", "-c", &owner_sql,
        ],
    )
    .await?;
    let mut parts = out.trim().split('|');
    let owner = parts.next().unwrap_or("").trim().to_string();
    let is_super = parts.next().map(|s| s.trim() == "t").unwrap_or(false);
    if owner.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.databases.owner_not_found",
            &[("name", &dbname)],
        )));
    }
    if is_super {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.databases.privileges_superuser",
            &[("name", &owner)],
        )));
    }

    let sql = format!(
        "ALTER ROLE {} WITH {}",
        pg_ident(&owner),
        pg_role_flags(body.createrole, body.createdb)
    );
    exec_checked(
        &state.docker,
        &container,
        &["psql", "-U", "postgres", "-c", &sql],
    )
    .await?;

    tracing::info!(
        "Set privileges for role {owner} (db: {dbname}) in {container}: createrole={} createdb={}",
        body.createrole,
        body.createdb
    );
    Ok(Json(serde_json::json!({
        "ok": true,
        "owner": owner,
        "createrole": body.createrole,
        "createdb": body.createdb,
    })))
}

/// psql `-A` prints booleans as `t` / `f`; anything else (missing column on
/// non-PostgreSQL engines) is `null`.
fn pg_bool(field: Option<&&str>) -> serde_json::Value {
    match field.map(|s| s.trim()) {
        Some("t") => serde_json::Value::Bool(true),
        Some("f") => serde_json::Value::Bool(false),
        _ => serde_json::Value::Null,
    }
}

/// SQL string literal: wrap in single quotes, doubling any inside.
fn pg_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Quoted identifier for a role name read back from `pg_roles` — it may
/// carry upper case or characters an unquoted name would mangle.
fn pg_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn pg_role_flags(createrole: bool, createdb: bool) -> String {
    format!(
        "{} {}",
        if createrole {
            "CREATEROLE"
        } else {
            "NOCREATEROLE"
        },
        if createdb { "CREATEDB" } else { "NOCREATEDB" }
    )
}

/// Statement that leaves `username` able to log in with `password`.
///
/// A new role gets exactly the requested privileges. An existing one gets the
/// new password and only *gains* privileges: creating a second database for a
/// role must not quietly revoke something it already relies on — revoking is
/// done explicitly from the privileges toggle.
fn pg_role_sql(
    exists: bool,
    username: &str,
    password: &str,
    createrole: bool,
    createdb: bool,
) -> String {
    let pwd = pg_literal(password);
    if exists {
        let mut sql = format!("ALTER ROLE {username} WITH LOGIN PASSWORD {pwd}");
        if createrole {
            sql.push_str(" CREATEROLE");
        }
        if createdb {
            sql.push_str(" CREATEDB");
        }
        sql
    } else {
        format!(
            "CREATE ROLE {username} WITH LOGIN PASSWORD {pwd} {}",
            pg_role_flags(createrole, createdb)
        )
    }
}

/// Upsert the stored credentials for `(service_id, db_name)` and carry the
/// new password to every other database of the same user. PostgreSQL and
/// MySQL users are server-wide, so one password change applies to all of
/// them; leaving the siblings stale is how the panel ended up showing
/// passwords that no longer worked. MongoDB users are per-database, so a
/// shared username there is a different user.
fn store_credentials(
    db: &rusqlite::Connection,
    service_id: &str,
    db_name: &str,
    username: &str,
    password: &str,
    server_wide_user: bool,
) -> Result<(), AppError> {
    let updated = db.execute(
        "UPDATE database_credentials SET username = ?1, password = ?2 \
         WHERE service_id = ?3 AND db_name = ?4",
        rusqlite::params![username, password, service_id, db_name],
    )?;
    if updated == 0 {
        db.execute(
            "INSERT INTO database_credentials (id, service_id, db_name, username, password) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                uuid::Uuid::new_v4().to_string(),
                service_id,
                db_name,
                username,
                password
            ],
        )?;
    }
    if server_wide_user {
        db.execute(
            "UPDATE database_credentials SET password = ?1 WHERE service_id = ?2 AND username = ?3",
            rusqlite::params![password, service_id, username],
        )?;
    }
    Ok(())
}

/// Execute a command inside a Docker container and return stdout+stderr.
///
/// The exit code is ignored — callers that parse whatever the tool printed
/// (mongosh browsing) rely on that. Anything that changes state should use
/// [`exec_checked`] instead.
pub(crate) async fn exec_in_container(
    docker: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<String, AppError> {
    exec_with_exit_code(docker, container, cmd)
        .await
        .map(|(out, _)| out)
}

/// Like [`exec_in_container`], but a non-zero exit code is an error carrying
/// the tool's own message. Without this a failed `CREATE USER` (role already
/// there) was reported as success and the panel stored a password the
/// database never got.
pub(crate) async fn exec_checked(
    docker: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<String, AppError> {
    let (out, code) = exec_with_exit_code(docker, container, cmd).await?;
    match code {
        Some(0) | None => Ok(out),
        Some(_) => Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.databases.command_failed",
            &[("error", out.trim())],
        ))),
    }
}

async fn exec_with_exit_code(
    docker: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<(String, Option<i64>), AppError> {
    use bollard::exec::{CreateExecOptions, StartExecResults};
    use futures_util::StreamExt;

    let exec = docker
        .create_exec(
            container,
            CreateExecOptions {
                cmd: Some(cmd.iter().map(|s| s.to_string()).collect()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .map_err(|e| {
            if e.to_string().contains("404") || e.to_string().contains("No such container") {
                AppError::BadRequest(crate::i18n::te_args(
                    "errors.databases.container_not_found",
                    &[("name", container)],
                ))
            } else {
                AppError::Internal(anyhow::anyhow!("Docker exec: {e}"))
            }
        })?;

    let output = docker
        .start_exec(&exec.id, None)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Docker exec start: {e}")))?;

    let mut result = String::new();
    if let StartExecResults::Attached { mut output, .. } = output {
        while let Some(Ok(msg)) = output.next().await {
            result.push_str(&msg.to_string());
        }
    }

    // The stream has ended, so the process has exited and inspect reports
    // its code. `None` only if Docker cannot say — treat that as success
    // rather than failing an operation that may well have worked.
    let exit_code = docker
        .inspect_exec(&exec.id)
        .await
        .ok()
        .and_then(|i| i.exit_code);

    Ok((result, exit_code))
}

/// Install the requested PostGIS extensions into the freshly-created database.
/// `requested` is filtered through `POSTGIS_EXTENSIONS` to reject anything
/// that isn't on the allowlist; ordering follows `POSTGIS_EXTENSIONS` so deps
/// land first. `CASCADE` lets PostgreSQL pull in transitive deps if needed.
async fn install_postgis_extensions(
    docker: &bollard::Docker,
    container: &str,
    db_name: &str,
    requested: &[String],
) -> Result<(), AppError> {
    for ext in POSTGIS_EXTENSIONS {
        if !requested.iter().any(|r| r == ext) {
            continue;
        }
        let sql = format!("CREATE EXTENSION IF NOT EXISTS {ext} CASCADE");
        exec_in_container(
            docker,
            container,
            &["psql", "-U", "postgres", "-d", db_name, "-c", &sql],
        )
        .await?;
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct CreateDatabaseRequest {
    pub database: String,
    pub username: String,
    pub password: String,
    /// PostgreSQL-family only: let the owner create roles (`CREATEROLE`).
    /// Apps whose migrations set up their own `*_app` / `*_owner` roles need it.
    #[serde(default)]
    pub createrole: bool,
    /// PostgreSQL-family only: let the owner create databases (`CREATEDB`).
    #[serde(default)]
    pub createdb: bool,
    /// PostGIS-only: list of PostGIS extensions to install in the new DB.
    /// Each name is validated against `POSTGIS_EXTENSIONS`. Ignored for
    /// non-postgis catalogs.
    #[serde(default)]
    pub extensions: Vec<String>,
}

/// Whitelist of PostGIS extensions installable through the UI. Order is
/// significant: dependencies first so `CREATE EXTENSION ... CASCADE` always
/// has its prerequisites in place.
const POSTGIS_EXTENSIONS: &[&str] = &[
    "postgis",
    "postgis_topology",
    "postgis_raster",
    "fuzzystrmatch",
    "postgis_tiger_geocoder",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_literal_doubles_single_quotes() {
        assert_eq!(pg_literal("it's"), "'it''s'");
        assert_eq!(pg_literal("plain"), "'plain'");
    }

    #[test]
    fn new_role_gets_exactly_the_requested_privileges() {
        assert_eq!(
            pg_role_sql(false, "sudoku", "pw", true, false),
            "CREATE ROLE sudoku WITH LOGIN PASSWORD 'pw' CREATEROLE NOCREATEDB"
        );
        assert_eq!(
            pg_role_sql(false, "app", "pw", false, false),
            "CREATE ROLE app WITH LOGIN PASSWORD 'pw' NOCREATEROLE NOCREATEDB"
        );
    }

    #[test]
    fn existing_role_gets_password_and_only_gains_privileges() {
        // Creating a second database must never revoke what the role has.
        assert_eq!(
            pg_role_sql(true, "sudoku", "pw", false, false),
            "ALTER ROLE sudoku WITH LOGIN PASSWORD 'pw'"
        );
        assert_eq!(
            pg_role_sql(true, "sudoku", "pw", true, true),
            "ALTER ROLE sudoku WITH LOGIN PASSWORD 'pw' CREATEROLE CREATEDB"
        );
    }

    #[test]
    fn password_with_quote_cannot_break_out_of_the_literal() {
        let sql = pg_role_sql(false, "u", "a'; DROP ROLE postgres; --", false, false);
        assert!(
            sql.contains("PASSWORD 'a''; DROP ROLE postgres; --'"),
            "{sql}"
        );
    }

    #[test]
    fn pg_ident_quotes_and_escapes() {
        assert_eq!(pg_ident("Sudoku"), "\"Sudoku\"");
        assert_eq!(pg_ident("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn pg_bool_parses_psql_booleans() {
        assert_eq!(pg_bool(Some(&"t")), serde_json::Value::Bool(true));
        assert_eq!(pg_bool(Some(&"f ")), serde_json::Value::Bool(false));
        assert_eq!(pg_bool(None), serde_json::Value::Null);
    }

    #[test]
    fn store_credentials_upserts_and_syncs_server_wide_user() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::schema::run_migrations(&conn).unwrap();
        conn.execute(
            "INSERT INTO services (id, name, service_type) VALUES ('s', 'pg', 'database')",
            [],
        )
        .unwrap();
        store_credentials(&conn, "s", "one", "sudoku", "old", true).unwrap();
        store_credentials(&conn, "s", "two", "sudoku", "new", true).unwrap();
        let pw: Vec<String> = conn
            .prepare("SELECT password FROM database_credentials ORDER BY db_name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(pw, vec!["new".to_string(), "new".to_string()]);

        // Re-storing the same database updates, never duplicates.
        store_credentials(&conn, "s", "two", "sudoku", "newer", false).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM database_credentials WHERE db_name = 'two'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }
}
