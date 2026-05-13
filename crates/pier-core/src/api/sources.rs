use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateSourceRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: String,
    pub url: String,
    #[serde(default)]
    pub token: String,
    // GitHub App fields
    pub app_id: Option<String>,
    pub installation_id: Option<i64>,
    pub private_key: Option<String>,
    // Project binding
    pub project_id: Option<String>,
}

/// GET /api/v1/sources
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, source_type, base_url, created_at
         FROM git_sources WHERE is_active = 1 ORDER BY created_at DESC",
    )?;
    let items: Vec<serde_json::Value> = stmt
        .query_map([], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "type": row.get::<_, String>(2)?,
                "url": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(items))
}

/// POST /api/v1/sources
pub async fn create(
    State(state): State<SharedState>,
    Json(body): Json<CreateSourceRequest>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.url.trim().is_empty() {
        return Err(AppError::BadRequest("Name and URL are required".into()));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let token = if body.token.is_empty() {
        None
    } else {
        Some(body.token.as_str())
    };
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO git_sources (id, name, source_type, base_url, access_token, app_id, installation_id, private_key, project_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            id,
            body.name.trim(),
            body.source_type,
            body.url.trim(),
            token,
            body.app_id,
            body.installation_id,
            body.private_key,
            body.project_id
        ],
    )?;
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

/// DELETE /api/v1/sources/{id}
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = db.execute("DELETE FROM git_sources WHERE id = ?1", [&id])?;
    if rows == 0 {
        return Err(AppError::NotFound(format!("Source {id} not found")));
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

/// GET /api/v1/sources/{id}/repos
pub async fn list_repos(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (source_type, base_url, access_token, app_id, installation_id, private_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT source_type, base_url, access_token, app_id, installation_id, private_key FROM git_sources WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Source {id} not found")))?
    };

    let repos = if source_type == "github-app" {
        let app_id = app_id.ok_or_else(|| AppError::BadRequest("Missing app_id".into()))?;
        let inst_id = installation_id
            .ok_or_else(|| AppError::BadRequest("Missing installation_id".into()))?;
        let pk = private_key.ok_or_else(|| AppError::BadRequest("Missing private_key".into()))?;
        crate::git::github_app::list_repos(&app_id, inst_id, &pk)
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to fetch repos: {e}")))?
    } else {
        crate::git::list_repos(&source_type, &base_url, access_token.as_deref())
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to fetch repos: {e}")))?
    };

    Ok(Json(serde_json::json!(repos)))
}

/// GET /api/v1/sources/{id}/repos/{repo}/branches — list branches for a repo.
pub async fn list_branches(
    State(state): State<SharedState>,
    Path((id, repo)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    let (app_id, installation_id, private_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT app_id, installation_id, private_key FROM git_sources WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Source {id} not found")))?
    };

    let app_id = app_id.ok_or_else(|| AppError::BadRequest("Missing app_id".into()))?;
    let inst_id =
        installation_id.ok_or_else(|| AppError::BadRequest("Missing installation_id".into()))?;
    let pk = private_key.ok_or_else(|| AppError::BadRequest("Missing private_key".into()))?;

    // repo comes as path param — Axum already decodes it
    let repo_name = &repo;

    let branches = crate::git::github_app::list_branches(&app_id, inst_id, &pk, repo_name)
        .await
        .map_err(|e| AppError::BadRequest(format!("Failed to list branches: {e}")))?;

    Ok(Json(serde_json::json!(branches)))
}

/// GET /api/v1/sources/{id} — source detail (for source detail page)
pub async fn get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let source = db
        .query_row(
            "SELECT id, name, source_type, base_url, app_id, installation_id, client_id, webhook_secret, project_id, created_at, client_secret
             FROM git_sources WHERE id = ?1",
            [&id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "type": row.get::<_, String>(2)?,
                    "url": row.get::<_, String>(3)?,
                    "app_id": row.get::<_, Option<String>>(4)?,
                    "installation_id": row.get::<_, Option<i64>>(5)?,
                    "client_id": row.get::<_, Option<String>>(6)?,
                    "webhook_secret": row.get::<_, Option<String>>(7)?,
                    "project_id": row.get::<_, Option<String>>(8)?,
                    "created_at": row.get::<_, String>(9)?,
                    "client_secret": row.get::<_, Option<String>>(10)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Source {id} not found")))?;

    // Get resources using this source
    let mut stmt =
        db.prepare("SELECT id, name, status, catalog_id FROM services WHERE git_source_id = ?1")?;
    let resources: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "status": row.get::<_, String>(2)?,
                "catalog_id": row.get::<_, Option<String>>(3)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(serde_json::json!({
        "source": source,
        "resources": resources,
    })))
}

/// GET /api/v1/sources/github/manifest — generate manifest + redirect info
pub async fn github_manifest(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    // Resolve the public-facing base URL for the App's webhook + callback,
    // along with the right `insecure_ssl` flag for the webhook spec.
    //
    //   - `platform_domain` set → Traefik terminates a valid Let's Encrypt
    //     cert in front of us, so verification stays on.
    //   - Otherwise → we expose the panel directly on its public IP. Scheme
    //     follows `tls_mode`: SelfSigned listens HTTPS (and GitHub won't trust
    //     a self-signed cert by default, so set insecure_ssl=1 for this hook
    //     only); Off listens plain HTTP, no TLS involved.
    let (platform_url, insecure_ssl) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let domain = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.platform_domain'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default();
        let ip = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'server.public_ip'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default();

        if !domain.is_empty() {
            (format!("https://{domain}"), "0")
        } else if !ip.is_empty() {
            match state.config.tls_mode {
                crate::config::TlsMode::SelfSigned => {
                    (format!("https://{ip}:{}", state.config.port), "1")
                }
                crate::config::TlsMode::Off => (format!("http://{ip}:{}", state.config.port), "0"),
            }
        } else {
            return Err(AppError::BadRequest(
                "Configure a platform domain in Proxy settings first for GitHub App OAuth flow"
                    .into(),
            ));
        }
    };

    let app_name = format!("pier-{}", &uuid::Uuid::new_v4().to_string()[..8]);
    let manifest = crate::git::github_app::generate_manifest(&platform_url, &app_name);

    Ok(Json(serde_json::json!({
        "manifest": manifest,
        "redirect_url": "https://github.com/settings/apps/new",
        // Surfaced into the callback so we can apply the right insecure_ssl
        // post-create (GitHub doesn't accept that field in the manifest).
        "insecure_ssl": insecure_ssl,
        "pier_url": platform_url,
    })))
}

/// Re-derive the `insecure_ssl` flag for the App's webhook from the same
/// inputs `github_manifest` used. Kept in one place so manifest generation
/// and the post-create patch stay in sync.
fn resolve_webhook_insecure_ssl(state: &SharedState) -> &'static str {
    let db = match state.db.lock() {
        Ok(g) => g,
        Err(_) => return "0",
    };
    let domain: String = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.platform_domain'",
            [],
            |row| row.get(0),
        )
        .unwrap_or_default();
    if !domain.is_empty() {
        return "0";
    }
    match state.config.tls_mode {
        crate::config::TlsMode::SelfSigned => "1",
        crate::config::TlsMode::Off => "0",
    }
}

/// GET /api/v1/sources/github/callback?code=CODE — GitHub App Manifest callback
pub async fn github_callback(
    State(state): State<SharedState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let code = match params.get("code") {
        Some(c) => c.clone(),
        None => {
            return axum::response::Redirect::to("/sources?error=missing_code");
        }
    };

    // Exchange code for app credentials
    match crate::git::github_app::exchange_manifest_code(&code).await {
        Ok(result) => {
            // Save to database
            let id = uuid::Uuid::new_v4().to_string();
            let name = format!("GitHub App: {}", result.slug);

            if let Ok(db) = state.db.lock() {
                let _ = db.execute(
                    "INSERT INTO git_sources (id, name, source_type, base_url, app_id, private_key, webhook_secret, client_id, client_secret)
                     VALUES (?1, ?2, 'github-app', 'https://github.com', ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        id, name, result.app_id, result.pem, result.webhook_secret, result.client_id, result.client_secret
                    ],
                );
                tracing::info!(
                    "GitHub App created via manifest flow: {} ({})",
                    result.slug,
                    result.app_id
                );
            }

            // If the panel is on self-signed TLS, GitHub would reject webhook
            // deliveries with `x509: certificate signed by unknown authority`.
            // Patch the App's webhook config right after creation so the very
            // first push works without the operator touching GitHub settings.
            let insecure_ssl = resolve_webhook_insecure_ssl(&state);
            if insecure_ssl == "1" {
                match crate::git::github_app::set_app_webhook_insecure_ssl(
                    &result.app_id,
                    &result.pem,
                    insecure_ssl,
                )
                .await
                {
                    Ok(()) => tracing::info!(
                        "GitHub App {} webhook insecure_ssl set to 1 (self-signed panel)",
                        result.app_id
                    ),
                    Err(e) => tracing::warn!(
                        "Could not patch webhook insecure_ssl for App {}: {e} — operator may need to disable SSL verification manually",
                        result.app_id
                    ),
                }
            }

            // Redirect to GitHub App install page to select repositories
            let install_url = format!("https://github.com/apps/{}/installations/new", result.slug);
            axum::response::Redirect::to(&install_url)
        }
        Err(e) => {
            tracing::error!("GitHub manifest exchange failed: {e}");
            axum::response::Redirect::to("/sources?error=exchange_failed")
        }
    }
}

/// GET /api/v1/sources/{id}/file?repo=user/repo&branch=main&path=docker-compose.yml
pub async fn get_file(
    State(state): State<SharedState>,
    Path(source_id): Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> AppResult<impl IntoResponse> {
    let repo = params
        .get("repo")
        .ok_or_else(|| AppError::BadRequest("repo is required".into()))?;
    let branch = params.get("branch").map(|s| s.as_str()).unwrap_or("main");
    let file_path = params
        .get("path")
        .map(|s| s.as_str())
        .unwrap_or("docker-compose.yml");

    let (app_id, installation_id, private_key) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT app_id, installation_id, private_key FROM git_sources WHERE id = ?1",
            [&source_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Source {source_id} not found")))?
    };

    let app_id = app_id.ok_or_else(|| AppError::BadRequest("Missing app_id".into()))?;
    let inst_id =
        installation_id.ok_or_else(|| AppError::BadRequest("Missing installation_id".into()))?;
    let pk = private_key.ok_or_else(|| AppError::BadRequest("Missing private_key".into()))?;

    let content =
        crate::git::github_app::get_file_content(&app_id, inst_id, &pk, repo, branch, file_path)
            .await
            .map_err(|e| AppError::Internal(anyhow::anyhow!("{e}")))?;

    Ok(Json(serde_json::json!({
        "content": content,
        "path": file_path,
    })))
}
