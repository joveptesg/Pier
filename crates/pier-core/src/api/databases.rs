use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

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
        .map_err(|_| AppError::NotFound(format!("Resource {service_id} not found")))?
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
pub async fn list_databases(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
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
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    let output = match catalog.as_str() {
        "postgresql" => {
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
                    "SELECT d.datname, r.rolname, pg_size_pretty(pg_database_size(d.datname)) FROM pg_database d JOIN pg_roles r ON d.datdba = r.oid WHERE d.datistemplate = false ORDER BY d.datname",
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
            return Err(AppError::BadRequest(
                "Database management only supported for PostgreSQL, MySQL, and MongoDB".into(),
            ));
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
            })
        })
        .filter(|d| {
            let name = d["name"].as_str().unwrap_or("");
            !name.is_empty() && name != "template0" && name != "template1"
        })
        .collect();

    Ok(Json(databases))
}

/// POST /api/v1/resources/{id}/databases — create database + user.
pub async fn create_database(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<CreateDatabaseRequest>,
) -> AppResult<impl IntoResponse> {
    let db_name = body.database.trim();
    let username = body.username.trim();
    let password = &body.password;

    if db_name.is_empty() || username.is_empty() || password.is_empty() {
        return Err(AppError::BadRequest(
            "Database name, username, and password are required".into(),
        ));
    }

    // Validate names (alphanumeric + underscore only)
    let valid = |s: &str| s.chars().all(|c| c.is_alphanumeric() || c == '_');
    if !valid(db_name) || !valid(username) {
        return Err(AppError::BadRequest(
            "Names may only contain letters, numbers, and underscores".into(),
        ));
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
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    match catalog.as_str() {
        "postgresql" => {
            // Each command must run separately — CREATE DATABASE cannot run inside a transaction
            let create_user = format!("CREATE USER {username} WITH PASSWORD '{password}'");
            exec_in_container(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &create_user],
            )
            .await?;

            let create_db = format!("CREATE DATABASE {db_name} OWNER {username}");
            exec_in_container(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &create_db],
            )
            .await?;

            let grant = format!("GRANT ALL PRIVILEGES ON DATABASE {db_name} TO {username}");
            exec_in_container(
                &state.docker,
                &container,
                &["psql", "-U", "postgres", "-c", &grant],
            )
            .await?;
        }
        "mysql" | "mariadb" => {
            let sql = format!(
                "CREATE DATABASE IF NOT EXISTS {db_name}; CREATE USER IF NOT EXISTS '{username}'@'%' IDENTIFIED BY '{password}'; GRANT ALL PRIVILEGES ON {db_name}.* TO '{username}'@'%'; FLUSH PRIVILEGES;"
            );
            exec_in_container(
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
            return Err(AppError::BadRequest("Unsupported database type".into()));
        }
    }

    // Store credentials in database_credentials table
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cred_id = uuid::Uuid::new_v4().to_string();
        let _ = db.execute(
            "INSERT INTO database_credentials (id, service_id, db_name, username, password) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![cred_id, id, db_name, username, password],
        );
    }

    tracing::info!("Created database {db_name} with user {username} in {container}");

    Ok(Json(serde_json::json!({
        "ok": true,
        "database": db_name,
        "username": username,
    })))
}

/// DELETE /api/v1/resources/{id}/databases/{dbname} — drop database + user.
pub async fn delete_database(
    State(state): State<SharedState>,
    Path((id, dbname)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
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
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    if matches!(
        dbname.as_str(),
        "postgres" | "mysql" | "information_schema" | "admin" | "local" | "config"
    ) {
        return Err(AppError::BadRequest(
            "Cannot delete system databases".into(),
        ));
    }

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    match catalog.as_str() {
        "postgresql" => {
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
            return Err(AppError::BadRequest("Unsupported database type".into()));
        }
    }

    // Remove stored credentials
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let _ = db.execute(
            "DELETE FROM database_credentials WHERE service_id = ?1 AND db_name = ?2",
            rusqlite::params![id, dbname],
        );
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
    Path((id, dbname)): Path<(String, String)>,
    Json(body): Json<ChangePasswordRequest>,
) -> AppResult<impl IntoResponse> {
    let password = body.password.trim();
    if password.is_empty() {
        return Err(AppError::BadRequest("Password is required".into()));
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
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
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
            "postgresql" => {
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
        return Err(AppError::BadRequest(format!(
            "Could not find owner for database {dbname}"
        )));
    }

    match catalog.as_str() {
        "postgresql" => {
            let sql = format!("ALTER USER {username} WITH PASSWORD '{password}'");
            exec_in_container(
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
            exec_in_container(
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
        _ => return Err(AppError::BadRequest("Unsupported database type".into())),
    }

    // Upsert stored credentials
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let updated = db.execute(
            "UPDATE database_credentials SET password = ?1 WHERE service_id = ?2 AND db_name = ?3",
            rusqlite::params![password, id, dbname],
        ).unwrap_or(0);
        if updated == 0 {
            // No existing record — insert new one
            let cred_id = uuid::Uuid::new_v4().to_string();
            let _ = db.execute(
                "INSERT INTO database_credentials (id, service_id, db_name, username, password) VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![cred_id, id, dbname, username, password],
            );
        }
    }

    tracing::info!("Changed password for user {username} (db: {dbname}) in {container}");
    Ok(Json(serde_json::json!({"ok": true})))
}

/// Execute a command inside a Docker container and return stdout.
async fn exec_in_container(
    docker: &bollard::Docker,
    container: &str,
    cmd: &[&str],
) -> Result<String, AppError> {
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
                AppError::BadRequest(format!(
                    "Container '{container}' not found. Make sure the service is running."
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

    Ok(result)
}

#[derive(Deserialize)]
pub struct CreateDatabaseRequest {
    pub database: String,
    pub username: String,
    pub password: String,
}
