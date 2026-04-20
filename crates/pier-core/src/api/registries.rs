use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::crypto;
use crate::docker::auth::parse_registry_host;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct ListQuery {
    pub project_id: Option<String>,
    #[serde(default)]
    pub global: Option<bool>,
}

#[derive(Deserialize)]
pub struct CreateRegistryRequest {
    pub registry: String,
    pub username: String,
    pub password: String,
    pub label: Option<String>,
    pub project_id: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateRegistryRequest {
    pub username: Option<String>,
    pub password: Option<String>,
    pub label: Option<String>,
}

#[derive(Deserialize)]
pub struct TestQuery {
    pub image: Option<String>,
}

/// GET /api/v1/registries
/// - `?project_id=<id>` → only rows for that project (without global).
/// - `?global=true`     → only global rows.
/// - no query           → every row visible to the caller.
pub async fn list(
    State(state): State<SharedState>,
    Query(q): Query<ListQuery>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let (sql, params): (&str, Vec<Box<dyn rusqlite::types::ToSql>>) =
        match (&q.project_id, q.global) {
            (Some(pid), _) => (
                "SELECT id, project_id, registry, username, label, created_at, updated_at
             FROM registry_credentials
             WHERE project_id = ?1
             ORDER BY registry",
                vec![Box::new(pid.clone())],
            ),
            (None, Some(true)) => (
                "SELECT id, project_id, registry, username, label, created_at, updated_at
             FROM registry_credentials
             WHERE project_id IS NULL
             ORDER BY registry",
                vec![],
            ),
            _ => (
                "SELECT id, project_id, registry, username, label, created_at, updated_at
             FROM registry_credentials
             ORDER BY (project_id IS NULL) DESC, registry",
                vec![],
            ),
        };

    let mut stmt = db.prepare(sql)?;
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows: Vec<serde_json::Value> = stmt
        .query_map(param_refs.as_slice(), |row| {
            let project_id: Option<String> = row.get(1)?;
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "project_id": project_id,
                "is_global": project_id.is_none(),
                "registry": row.get::<_, String>(2)?,
                "username": row.get::<_, String>(3)?,
                "label": row.get::<_, Option<String>>(4)?,
                "created_at": row.get::<_, String>(5)?,
                "updated_at": row.get::<_, String>(6)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(rows))
}

/// POST /api/v1/registries
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateRegistryRequest>,
) -> AppResult<impl IntoResponse> {
    let registry = body.registry.trim().to_lowercase();
    let username = body.username.trim();
    if registry.is_empty() || username.is_empty() || body.password.is_empty() {
        return Err(AppError::BadRequest(
            "registry, username and password are required".into(),
        ));
    }

    let key = crypto::get_secret_key();
    let password_enc = crypto::encrypt(&body.password, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt password: {e}")))?;

    let id = uuid::Uuid::new_v4().to_string();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    db.execute(
        "INSERT INTO registry_credentials (id, project_id, registry, username, password_enc, label)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            id,
            body.project_id,
            registry,
            username,
            password_enc,
            body.label
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            AppError::Conflict(format!(
                "Credentials for registry '{registry}' already exist in this scope"
            ))
        }
        other => AppError::Database(other),
    })?;

    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

/// PUT /api/v1/registries/:id
pub async fn update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRegistryRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut sets = vec!["updated_at = datetime('now')".to_string()];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();

    if let Some(username) = body.username.as_deref() {
        sets.push(format!("username = ?{}", params.len() + 1));
        params.push(Box::new(username.trim().to_string()));
    }
    if let Some(label) = &body.label {
        sets.push(format!("label = ?{}", params.len() + 1));
        params.push(Box::new(label.clone()));
    }
    if let Some(pw) = body.password.as_deref() {
        if !pw.is_empty() {
            let key = crypto::get_secret_key();
            let enc = crypto::encrypt(pw, &key)
                .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt password: {e}")))?;
            sets.push(format!("password_enc = ?{}", params.len() + 1));
            params.push(Box::new(enc));
        }
    }

    params.push(Box::new(id.clone()));
    let sql = format!(
        "UPDATE registry_credentials SET {} WHERE id = ?{}",
        sets.join(", "),
        params.len()
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = db.execute(&sql, param_refs.as_slice())?;
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "Registry credential {id} not found"
        )));
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/v1/registries/:id
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM registry_credentials WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!(
            "Registry credential {id} not found"
        )));
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// POST /api/v1/registries/:id/test?image=<optional>
///
/// Tries a real `create_image` using the stored credentials. If the caller
/// doesn't supply `?image=`, we probe a registry-dependent default tag.
pub async fn test(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Query(q): Query<TestQuery>,
) -> AppResult<impl IntoResponse> {
    use bollard::auth::DockerCredentials;
    use bollard::query_parameters::CreateImageOptions;
    use futures_util::StreamExt;

    let (registry, username, password_enc) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT registry, username, password_enc FROM registry_credentials WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Registry credential {id} not found")))?
    };

    let key = crypto::get_secret_key();
    let password = crypto::decrypt(&password_enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt password: {e}")))?;

    let probe_image = q
        .image
        .clone()
        .unwrap_or_else(|| default_probe_image(&registry));
    if parse_registry_host(&probe_image) != registry {
        return Err(AppError::BadRequest(format!(
            "Probe image '{probe_image}' does not belong to registry '{registry}'"
        )));
    }

    let creds = DockerCredentials {
        username: Some(username),
        password: Some(password),
        serveraddress: Some(registry.clone()),
        ..Default::default()
    };

    let opts = CreateImageOptions {
        from_image: Some(probe_image.clone()),
        ..Default::default()
    };

    let mut stream = state.docker.create_image(Some(opts), None, Some(creds));
    let mut last_err: Option<String> = None;
    while let Some(item) = stream.next().await {
        if let Err(e) = item {
            last_err = Some(e.to_string());
            break;
        }
    }

    match last_err {
        None => Ok(Json(serde_json::json!({
            "ok": true,
            "registry": registry,
            "probe_image": probe_image,
        }))),
        Some(err) => Ok(Json(serde_json::json!({
            "ok": false,
            "registry": registry,
            "probe_image": probe_image,
            "error": err,
        }))),
    }
}

/// Default image used by the connection test when the caller didn't provide one.
fn default_probe_image(registry: &str) -> String {
    match registry {
        "docker.io" => "library/hello-world:latest".into(),
        _ => format!("{registry}/library/hello-world:latest"),
    }
}
