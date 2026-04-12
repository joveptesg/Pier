use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

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
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                ))
            },
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
        _ => {
            return Err(AppError::BadRequest(
                "Database management only supported for PostgreSQL and MySQL".into(),
            ));
        }
    };

    // Parse output into structured data
    let databases: Vec<serde_json::Value> = output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('|').collect();
            serde_json::json!({
                "name": parts.first().map(|s| s.trim()).unwrap_or(""),
                "owner": parts.get(1).map(|s| s.trim()).unwrap_or(""),
                "size": parts.get(2).map(|s| s.trim()).unwrap_or("0"),
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
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    let container = format!("pier-{}", name.to_lowercase().replace(' ', "-"));
    let catalog = catalog_id.unwrap_or_default();

    match catalog.as_str() {
        "postgresql" => {
            // Each command must run separately — CREATE DATABASE cannot run inside a transaction
            let create_user = format!("CREATE USER {username} WITH PASSWORD '{password}'");
            exec_in_container(&state.docker, &container, &["psql", "-U", "postgres", "-c", &create_user]).await?;

            let create_db = format!("CREATE DATABASE {db_name} OWNER {username}");
            exec_in_container(&state.docker, &container, &["psql", "-U", "postgres", "-c", &create_db]).await?;

            let grant = format!("GRANT ALL PRIVILEGES ON DATABASE {db_name} TO {username}");
            exec_in_container(&state.docker, &container, &["psql", "-U", "postgres", "-c", &grant]).await?;
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
        _ => {
            return Err(AppError::BadRequest("Unsupported database type".into()));
        }
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
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Resource {id} not found")))?
    };

    if dbname == "postgres" || dbname == "mysql" || dbname == "information_schema" {
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
        _ => {
            return Err(AppError::BadRequest("Unsupported database type".into()));
        }
    }

    tracing::info!("Deleted database {dbname} from {container}");
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
                AppError::BadRequest(format!("Container '{container}' not found. Make sure the service is running."))
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
