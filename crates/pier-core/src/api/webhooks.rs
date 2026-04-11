use std::sync::Arc;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::deploy::{self, CommitInfo};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

type HmacSha256 = Hmac<Sha256>;

/// POST /api/v1/webhooks/github — receive GitHub push webhook.
pub async fn github(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> AppResult<impl IntoResponse> {
    // Parse event type
    let event = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Handle installation events (GitHub App installed/updated)
    if event == "installation" || event == "installation_repositories" {
        let payload: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {e}")))?;

        let action = payload["action"].as_str().unwrap_or("");
        if action == "created" || action == "added" {
            let installation_id = payload["installation"]["id"].as_i64().unwrap_or(0);
            let app_id = payload["installation"]["app_id"].as_i64().unwrap_or(0);

            if installation_id > 0 && app_id > 0 {
                if let Ok(db) = state.db.lock() {
                    let rows = db.execute(
                        "UPDATE git_sources SET installation_id = ?1 WHERE app_id = ?2 AND source_type = 'github-app'",
                        rusqlite::params![installation_id, app_id.to_string()],
                    ).unwrap_or(0);
                    if rows > 0 {
                        tracing::info!("GitHub App installation_id {installation_id} saved for app {app_id}");
                    }
                }
            }
        }

        return Ok(Json(serde_json::json!({"ok": true, "event": "installation"})));
    }

    if event != "push" {
        return Ok(Json(
            serde_json::json!({"ok": true, "skipped": "not a push event"}),
        ));
    }

    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Parse payload
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {e}")))?;

    let repo_url = payload["repository"]["html_url"]
        .as_str()
        .or_else(|| payload["repository"]["clone_url"].as_str())
        .unwrap_or("");

    let full_ref = payload["ref"].as_str().unwrap_or("");
    let branch = full_ref.strip_prefix("refs/heads/").unwrap_or(full_ref);

    let commit_sha = payload["after"].as_str().unwrap_or("");
    let commit_message = payload["head_commit"]["message"].as_str().unwrap_or("push");

    if repo_url.is_empty() || commit_sha.is_empty() {
        return Err(AppError::BadRequest(
            "Missing repo URL or commit SHA".into(),
        ));
    }

    // Find matching service
    let service = find_service_by_repo(&state, repo_url, branch)?;

    let (service_id, webhook_secret) = match service {
        Some(s) => s,
        None => {
            tracing::debug!("No matching service for {repo_url} branch {branch}");
            return Ok(Json(
                serde_json::json!({"ok": true, "skipped": "no matching service"}),
            ));
        }
    };

    // Verify signature
    if let Some(secret) = &webhook_secret {
        verify_github_signature(secret, &body, signature)?;
    }

    let commit = CommitInfo {
        sha: commit_sha.to_string(),
        message: commit_message.to_string(),
        branch: branch.to_string(),
    };

    // Spawn pipeline in background
    let state_clone = Arc::clone(&state);
    let sid = service_id.clone();
    tokio::spawn(async move {
        deploy::run_pipeline(state_clone, sid, commit, "webhook").await;
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "service_id": service_id,
        "message": "Deploy pipeline started",
    })))
}

/// POST /api/v1/webhooks/gitlab — receive GitLab push webhook.
pub async fn gitlab(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> AppResult<impl IntoResponse> {
    let event = headers
        .get("X-Gitlab-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if event != "Push Hook" {
        return Ok(Json(
            serde_json::json!({"ok": true, "skipped": "not a push event"}),
        ));
    }

    let gitlab_token = headers
        .get("X-Gitlab-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Parse payload
    let payload: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| AppError::BadRequest(format!("Invalid JSON: {e}")))?;

    let repo_url = payload["project"]["web_url"].as_str().unwrap_or("");

    let full_ref = payload["ref"].as_str().unwrap_or("");
    let branch = full_ref.strip_prefix("refs/heads/").unwrap_or(full_ref);

    let commit_sha = payload["after"].as_str().unwrap_or("");
    let commit_message = payload["commits"]
        .as_array()
        .and_then(|c| c.last())
        .and_then(|c| c["message"].as_str())
        .unwrap_or("push");

    if repo_url.is_empty() || commit_sha.is_empty() {
        return Err(AppError::BadRequest(
            "Missing repo URL or commit SHA".into(),
        ));
    }

    // Find matching service
    let service = find_service_by_repo(&state, repo_url, branch)?;

    let (service_id, webhook_secret) = match service {
        Some(s) => s,
        None => {
            return Ok(Json(
                serde_json::json!({"ok": true, "skipped": "no matching service"}),
            ));
        }
    };

    // Verify GitLab token
    if let Some(secret) = &webhook_secret {
        if gitlab_token != secret.as_str() {
            return Err(AppError::Unauthorized);
        }
    }

    let commit = CommitInfo {
        sha: commit_sha.to_string(),
        message: commit_message.to_string(),
        branch: branch.to_string(),
    };

    let state_clone = Arc::clone(&state);
    let sid = service_id.clone();
    tokio::spawn(async move {
        deploy::run_pipeline(state_clone, sid, commit, "webhook").await;
    });

    Ok(Json(serde_json::json!({
        "ok": true,
        "service_id": service_id,
        "message": "Deploy pipeline started",
    })))
}

/// Find a service matching the given repo URL and branch.
fn find_service_by_repo(
    state: &SharedState,
    repo_url: &str,
    branch: &str,
) -> AppResult<Option<(String, Option<String>)>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Normalize URL: strip trailing .git
    let normalized = repo_url.trim_end_matches(".git");

    let result = db.query_row(
        "SELECT id, git_webhook_secret FROM services
         WHERE (git_repo_url = ?1 OR git_repo_url = ?2 OR git_repo_url = ?3)
         AND (git_branch = ?4 OR git_branch IS NULL)
         LIMIT 1",
        rusqlite::params![repo_url, normalized, format!("{normalized}.git"), branch,],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
    );

    match result {
        Ok(row) => Ok(Some(row)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(AppError::Database(e)),
    }
}

/// Verify GitHub webhook HMAC-SHA256 signature.
fn verify_github_signature(secret: &str, body: &[u8], signature: &str) -> AppResult<()> {
    let expected_prefix = "sha256=";
    let hex_sig = signature
        .strip_prefix(expected_prefix)
        .ok_or_else(|| AppError::Unauthorized)?;

    let sig_bytes = hex::decode(hex_sig).map_err(|_| AppError::Unauthorized)?;

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| AppError::Unauthorized)?;
    mac.update(body);

    mac.verify_slice(&sig_bytes)
        .map_err(|_| AppError::Unauthorized)?;

    Ok(())
}
