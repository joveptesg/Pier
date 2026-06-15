//! HTTP API for Ad-hoc Tasks. All routes are admin-gated by the router
//! layer in [`super::mod`]; per-handler permission checks aren't repeated
//! here.

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use std::collections::HashMap;

use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;
use crate::tasks::{executor, models};

// ── Templates ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateTemplateRequest {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub command: String,
    #[serde(default)]
    pub default_timeout_sec: Option<i64>,
    #[serde(default)]
    pub default_env: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
pub struct UpdateTemplateRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub command: Option<String>,
    pub default_timeout_sec: Option<i64>,
    pub default_env: Option<serde_json::Map<String, serde_json::Value>>,
}

pub async fn templates_list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let items = models::template_list(&db).map_err(AppError::Internal)?;
    Ok(Json(items))
}

pub async fn templates_create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<CreateTemplateRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.tasks.name_required",
        )));
    }
    let command = body.command.trim().to_string();
    if command.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.tasks.command_required",
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let timeout = body.default_timeout_sec.unwrap_or(1800).clamp(1, 24 * 3600);
    let env_json = serde_json::to_string(&body.default_env.unwrap_or_default())
        .unwrap_or_else(|_| "{}".to_string());

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO task_templates
            (id, name, description, command, default_timeout_sec, default_env_json, created_by)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id,
            name,
            body.description,
            command,
            timeout,
            env_json,
            user.id,
        ],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(err, _)
            if err.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            AppError::Conflict(crate::i18n::te_args(
                "errors.tasks.template_name_exists",
                &[("v", &name)],
            ))
        }
        other => AppError::Internal(anyhow::anyhow!("insert template: {other}")),
    })?;
    Ok(Json(serde_json::json!({"ok": true, "id": id})))
}

pub async fn templates_get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let tmpl = models::template_get(&db, &id)
        .map_err(AppError::Internal)?
        .ok_or_else(|| AppError::NotFound(crate::i18n::te("errors.tasks.template_not_found")))?;
    Ok(Json(tmpl))
}

pub async fn templates_update(
    State(state): State<SharedState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTemplateRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    if let Some(name) = body.name {
        db.execute(
            "UPDATE task_templates SET name=?1, updated_at=datetime('now') WHERE id=?2",
            params![name.trim(), id],
        )?;
    }
    if let Some(d) = body.description {
        db.execute(
            "UPDATE task_templates SET description=?1, updated_at=datetime('now') WHERE id=?2",
            params![d, id],
        )?;
    }
    if let Some(c) = body.command {
        db.execute(
            "UPDATE task_templates SET command=?1, updated_at=datetime('now') WHERE id=?2",
            params![c.trim(), id],
        )?;
    }
    if let Some(t) = body.default_timeout_sec {
        db.execute(
            "UPDATE task_templates SET default_timeout_sec=?1, updated_at=datetime('now') WHERE id=?2",
            params![t.clamp(1, 24 * 3600), id],
        )?;
    }
    if let Some(env) = body.default_env {
        let env_json = serde_json::to_string(&env).unwrap_or_else(|_| "{}".to_string());
        db.execute(
            "UPDATE task_templates SET default_env_json=?1, updated_at=datetime('now') WHERE id=?2",
            params![env_json, id],
        )?;
    }
    Ok(Json(serde_json::json!({"ok": true})))
}

pub async fn templates_delete(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute("DELETE FROM task_templates WHERE id = ?1", [&id])?;
    Ok(Json(serde_json::json!({"ok": true})))
}

// ── Runs ────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StartRunRequest {
    pub server_id: String,
    /// Either a template_id (uses its command + env + timeout as defaults)
    /// or an inline command. If both are supplied, `command` wins.
    pub template_id: Option<String>,
    pub command: Option<String>,
    pub env: Option<serde_json::Map<String, serde_json::Value>>,
    pub timeout_sec: Option<i64>,
}

#[derive(Deserialize)]
pub struct ListRunsQuery {
    pub server_id: Option<String>,
    pub template_id: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i64>,
}

pub async fn runs_list(
    State(state): State<SharedState>,
    Query(q): Query<ListRunsQuery>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let rows = models::run_list(
        &db,
        q.server_id.as_deref(),
        q.template_id.as_deref(),
        q.status.as_deref(),
        q.limit.unwrap_or(50),
    )
    .map_err(AppError::Internal)?;
    Ok(Json(rows))
}

pub async fn runs_start(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Json(body): Json<StartRunRequest>,
) -> AppResult<impl IntoResponse> {
    // Resolve command + defaults: inline trumps template fields.
    let (command, mut env_map, mut timeout) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        if let Some(tid) = &body.template_id {
            let tmpl: Option<(String, String, i64)> = db
                .query_row(
                    "SELECT command, default_env_json, default_timeout_sec
                     FROM task_templates WHERE id = ?1",
                    [tid],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let (tmpl_cmd, env_str, tmpl_timeout) = tmpl.ok_or_else(|| {
                AppError::NotFound(crate::i18n::te("errors.tasks.template_not_found"))
            })?;
            let env_map: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&env_str).unwrap_or_default();
            (tmpl_cmd, env_map, tmpl_timeout)
        } else {
            (String::new(), serde_json::Map::new(), 1800i64)
        }
    };
    if let Some(c) = body.command {
        let c = c.trim().to_string();
        if !c.is_empty() {
            // Caller-supplied command wins.
            return run_with(
                &state,
                &user,
                body.server_id,
                body.template_id,
                c,
                body.env.unwrap_or(env_map),
                body.timeout_sec.unwrap_or(timeout),
            )
            .await;
        }
    }
    if command.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.tasks.template_or_command_required",
        )));
    }
    if let Some(env_override) = body.env {
        env_map = env_override;
    }
    if let Some(t_override) = body.timeout_sec {
        timeout = t_override;
    }
    run_with(
        &state,
        &user,
        body.server_id,
        body.template_id,
        command,
        env_map,
        timeout,
    )
    .await
}

async fn run_with(
    state: &SharedState,
    user: &AuthUser,
    server_id: String,
    template_id: Option<String>,
    command: String,
    env: serde_json::Map<String, serde_json::Value>,
    timeout_sec: i64,
) -> AppResult<axum::Json<serde_json::Value>> {
    let timeout_sec = timeout_sec.clamp(1, 24 * 3600);
    let task_id = executor::start_run(
        state,
        executor::StartSpec {
            server_id,
            template_id,
            command,
            env,
            timeout_sec,
            triggered_by: user.username.clone(),
        },
    )
    .await?;
    Ok(Json(serde_json::json!({"ok": true, "id": task_id})))
}

pub async fn runs_get(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let run = models::run_get(&db, &id)
        .map_err(AppError::Internal)?
        .ok_or_else(|| AppError::NotFound(crate::i18n::te("errors.tasks.run_not_found")))?;
    Ok(Json(run))
}

pub async fn runs_cancel(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    executor::cancel_run(&state, &id).await?;
    Ok(Json(serde_json::json!({"ok": true})))
}

#[allow(dead_code)] // surface for parity with future bulk-status helper
fn _ensure_compiles(_: HashMap<String, String>) {}
