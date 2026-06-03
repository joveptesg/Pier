use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Path, State};
use axum::http::header::USER_AGENT;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;

use crate::auth::audit::{self, AuthEvent};
use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::deploy::{self, rollback, CommitInfo};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

/// GET /api/v1/resources/{id}/deployments — list deployment history.
pub async fn list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt = db.prepare(
        "SELECT id, commit_sha, commit_message, branch, status, image_tag, triggered_by, duration_secs, started_at, finished_at
         FROM deployments WHERE service_id = ?1
         ORDER BY started_at DESC LIMIT 50",
    )?;

    let deployments: Vec<serde_json::Value> = stmt
        .query_map([&id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "commit_sha": row.get::<_, Option<String>>(1)?,
                "commit_message": row.get::<_, Option<String>>(2)?,
                "branch": row.get::<_, Option<String>>(3)?,
                "status": row.get::<_, String>(4)?,
                "image_tag": row.get::<_, Option<String>>(5)?,
                "triggered_by": row.get::<_, String>(6)?,
                "duration_secs": row.get::<_, Option<i64>>(7)?,
                "started_at": row.get::<_, String>(8)?,
                "finished_at": row.get::<_, Option<String>>(9)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(Json(deployments))
}

/// GET /api/v1/resources/{id}/deployments/{dep_id} — single deployment details.
pub async fn get(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dep_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Viewer)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let deployment = db
        .query_row(
            "SELECT id, commit_sha, commit_message, branch, status, build_log, image_tag, triggered_by, duration_secs, started_at, finished_at
             FROM deployments WHERE id = ?1 AND service_id = ?2",
            rusqlite::params![dep_id, id],
            |row| {
                Ok(serde_json::json!({
                    "id": row.get::<_, String>(0)?,
                    "commit_sha": row.get::<_, Option<String>>(1)?,
                    "commit_message": row.get::<_, Option<String>>(2)?,
                    "branch": row.get::<_, Option<String>>(3)?,
                    "status": row.get::<_, String>(4)?,
                    "build_log": row.get::<_, String>(5)?,
                    "image_tag": row.get::<_, Option<String>>(6)?,
                    "triggered_by": row.get::<_, String>(7)?,
                    "duration_secs": row.get::<_, Option<i64>>(8)?,
                    "started_at": row.get::<_, String>(9)?,
                    "finished_at": row.get::<_, Option<String>>(10)?,
                }))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Deployment {dep_id} not found")))?;

    Ok(Json(deployment))
}

/// POST /api/v1/resources/{id}/deployments/{dep_id}/cancel — mark an in-flight
/// deployment as cancelled. The underlying build task is not killed (we don't
/// track `JoinHandle`s); the guard in `finish_deployment` ensures it won't
/// resurrect the cancelled row.
pub async fn cancel(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path((id, dep_id)): Path<(String, String)>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let updated = db.execute(
        "UPDATE deployments
         SET status = 'cancelled', finished_at = datetime('now')
         WHERE id = ?1 AND service_id = ?2 AND status IN ('building', 'pending')",
        rusqlite::params![dep_id, id],
    )?;

    if updated == 0 {
        return Err(AppError::Conflict("Deployment is not in progress".into()));
    }

    // Clear the service's 'deploying' flag if no other deploy is active.
    let still_active: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM deployments
             WHERE service_id = ?1 AND status IN ('building', 'pending')",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if still_active == 0 {
        let _ = db.execute(
            "UPDATE services SET status = 'running', updated_at = datetime('now')
             WHERE id = ?1 AND status = 'deploying'",
            [&id],
        );
    }

    Ok(Json(serde_json::json!({"ok": true})))
}

/// POST /api/v1/resources/{id}/deploy — manual deploy trigger.
pub async fn manual_deploy(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ManualDeployRequest>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    // Verify service exists and has git configured
    let (git_repo_url, git_branch) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT git_repo_url, git_branch FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Service {id} not found")))?
    };

    if git_repo_url.is_none() || git_repo_url.as_deref() == Some("") {
        return Err(AppError::BadRequest(
            "Git is not configured for this service. Set git_repo_url first.".into(),
        ));
    }

    let branch = body
        .branch
        .filter(|b| !b.is_empty())
        .or(git_branch)
        .unwrap_or_else(|| "main".to_string());

    let commit = CommitInfo {
        sha: format!("manual-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        message: body
            .message
            .unwrap_or_else(|| "Manual deployment".to_string()),
        branch,
    };

    let state_clone = Arc::clone(&state);
    let sid = id.clone();
    tokio::spawn(async move {
        deploy::run_pipeline(state_clone, sid, commit, "manual").await;
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "message": "Deploy pipeline started",
    })))
}

/// POST /api/v1/resources/{id}/rollback — rollback to previous version.
pub async fn rollback(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    let state_clone = Arc::clone(&state);
    let sid = id.clone();

    match rollback::rollback_service(state_clone, sid).await {
        Ok(deploy_id) => Ok(Json(serde_json::json!({
            "ok": true,
            "deployment_id": deploy_id,
            "message": "Rollback completed",
        }))),
        Err(e) => Err(AppError::Internal(anyhow::anyhow!("{e}"))),
    }
}

#[derive(serde::Deserialize)]
pub struct ManualDeployRequest {
    pub branch: Option<String>,
    pub message: Option<String>,
}

/// Request body for the authenticated CI deploy API. All fields optional.
#[derive(serde::Deserialize)]
pub struct ApiDeployRequest {
    /// Real commit SHA to record (traceability). Absent → synthetic `api-…` id.
    pub commit_sha: Option<String>,
    /// Branch to deploy. `ref` is accepted as an alias for CI ergonomics.
    pub branch: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub message: Option<String>,
}

/// POST /api/v1/services/{id}/deploy — authenticated CI deploy by service id.
///
/// This is the integration seam for CI affected-pipelines (nx/turbo): CI
/// computes the affected set and calls this once per affected service. Auth is
/// a Bearer `api_token` (via `require_auth`); authorization is the token
/// owner's project RBAC (Editor). Returns the `deployment_id` so CI can poll
/// `status_url` until the build reaches a terminal state.
pub async fn api_deploy(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ApiDeployRequest>,
) -> AppResult<impl IntoResponse> {
    do_api_deploy(&state, &user, addr, &headers, id, body).await
}

/// POST /api/v1/projects/{project_id}/services/{name}/deploy — same as
/// [`api_deploy`] but resolves the service by (project, name), which is what CI
/// usually knows. Service names are unique within a project
/// (`idx_services_name_scope`).
pub async fn api_deploy_by_name(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((project_id, name)): Path<(String, String)>,
    Json(body): Json<ApiDeployRequest>,
) -> AppResult<impl IntoResponse> {
    let id = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT id FROM services WHERE project_id = ?1 AND name = ?2",
            rusqlite::params![project_id, name],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| AppError::NotFound(format!("Service '{name}' not found in project")))?
    };
    do_api_deploy(&state, &user, addr, &headers, id, body).await
}

/// Shared core for both CI deploy routes: gate, validate git, build the commit,
/// audit, and kick off the pipeline under a pre-generated id.
async fn do_api_deploy(
    state: &SharedState,
    user: &AuthUser,
    addr: SocketAddr,
    headers: &HeaderMap,
    id: String,
    body: ApiDeployRequest,
) -> AppResult<Json<serde_json::Value>> {
    enforce_resource_role(state, user, &id, ProjectRole::Editor)?;

    // Verify the service exists and has git configured (mirrors manual_deploy).
    let (git_repo_url, git_branch) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT git_repo_url, git_branch FROM services WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .map_err(|_| AppError::NotFound(format!("Service {id} not found")))?
    };
    if git_repo_url.as_deref().unwrap_or("").is_empty() {
        return Err(AppError::BadRequest(
            "Git is not configured for this service. Set git_repo_url first.".into(),
        ));
    }

    let branch = body
        .git_ref
        .or(body.branch)
        .filter(|b| !b.is_empty())
        .or(git_branch)
        .unwrap_or_else(|| "main".to_string());

    // Use the supplied commit SHA verbatim when present (so it's traceable and
    // gets recorded as last_deployed_sha); otherwise synthesize a marker id.
    let sha = body
        .commit_sha
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("api-{}", &uuid::Uuid::new_v4().to_string()[..8]));

    let commit = CommitInfo {
        sha: sha.clone(),
        message: body.message.unwrap_or_else(|| "API deploy".to_string()),
        branch,
    };

    // Pre-generate the deployment id so we can return it before the build runs.
    let deploy_id = uuid::Uuid::new_v4().to_string();

    // Audit the token-driven deploy ("who, when, from which IP").
    let ip = Some(addr.ip());
    let ua = headers.get(USER_AGENT).and_then(|v| v.to_str().ok());
    audit::log(
        state,
        AuthEvent::ServiceDeployed,
        Some(&user.id),
        ip,
        ua,
        Some(serde_json::json!({
            "service_id": id,
            "deployment_id": deploy_id,
            "via": "api_token",
            "commit_sha": sha,
        })),
    );

    let state_clone = Arc::clone(state);
    let sid = id.clone();
    let did = deploy_id.clone();
    tokio::spawn(async move {
        deploy::run_pipeline_with_id(state_clone, sid, commit, "api", did).await;
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "deployment_id": deploy_id,
        "service_id": id,
        "status_url": format!("/api/v1/resources/{id}/deployments/{deploy_id}"),
    })))
}
