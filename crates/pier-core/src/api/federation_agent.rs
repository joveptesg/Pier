//! Write-federation handlers — the surface a remote primary pier-core
//! calls when it wants to deploy / restart / inspect stacks on **this**
//! peer pier-core. Mounted at `/api/v1/agent/*` and gated by the
//! [`require_federation`](crate::auth::federation::require_federation)
//! middleware, so every request that reaches a handler has already
//! resolved a [`FederationContext`] in its extensions.
//!
//! Ownership model:
//! - `services.owner_server_id` is `NULL` for stacks created via this
//!   peer's own UI — those are off-limits to federation.
//! - `services.owner_server_id == ctx.token_id` means "this stack was
//!   created by the primary that owns the current token". Mutations
//!   are allowed.
//! - Anything else (owned by a different primary's token) returns 409
//!   Conflict so the caller learns it doesn't own the row.
//!
//! Mutations on existing locally-owned rows are NOT auto-claimed — the
//! peer's operator has to explicitly hand a stack over to a primary
//! (future endpoint; v2). The token alone authorises remote
//! orchestration of *future* stacks the primary creates, not seizure
//! of existing ones.

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Extension, Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::auth::federation::FederationContext;
use crate::docker;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateStackBody {
    pub name: String,
    pub yaml: String,
}

#[derive(Deserialize)]
pub struct UpdateStackBody {
    pub yaml: String,
}

/// GET /api/v1/agent/stacks
///
/// Returns only stacks owned by the current federation token. The
/// primary's own dashboard knows what it should see; locally-owned
/// stacks of the peer go through the read-federation surface
/// (`/api/v1/projects` and friends) instead.
pub async fn list_stacks(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut stmt = db.prepare(
        "SELECT id, name, compose_content, status, created_at \
         FROM services \
         WHERE service_type = 'compose' AND owner_server_id = ?1 \
         ORDER BY name",
    )?;
    let rows: Vec<serde_json::Value> = stmt
        .query_map([&ctx.token_id], |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "has_yaml": row.get::<_, Option<String>>(2)?.is_some(),
                "status": row.get::<_, String>(3)?,
                "created_at": row.get::<_, String>(4)?,
            }))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(Json(rows))
}

/// POST /api/v1/agent/stacks
pub async fn create_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Json(body): Json<CreateStackBody>,
) -> AppResult<impl IntoResponse> {
    if body.name.trim().is_empty() || body.yaml.trim().is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.federation_agent.name_and_yaml_required",
        )));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "INSERT INTO services \
            (id, name, service_type, compose_content, status, owner_server_id) \
         VALUES (?1, ?2, 'compose', ?3, 'created', ?4)",
        rusqlite::params![id, body.name.trim(), body.yaml, ctx.token_id],
    )?;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

/// GET /api/v1/agent/stacks/{id}
pub async fn get_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    assert_owned(&db, &id, &ctx.token_id)?;
    let row = db.query_row(
        "SELECT id, name, compose_content, status \
         FROM services \
         WHERE id = ?1 AND service_type = 'compose'",
        [&id],
        |row| {
            Ok(serde_json::json!({
                "id": row.get::<_, String>(0)?,
                "name": row.get::<_, String>(1)?,
                "yaml": row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                "status": row.get::<_, String>(3)?,
            }))
        },
    )?;
    Ok(Json(row))
}

/// PUT /api/v1/agent/stacks/{id}
pub async fn update_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
    Json(body): Json<UpdateStackBody>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    assert_owned(&db, &id, &ctx.token_id)?;
    db.execute(
        "UPDATE services SET compose_content = ?1, updated_at = datetime('now') \
         WHERE id = ?2 AND service_type = 'compose'",
        rusqlite::params![body.yaml, id],
    )?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// DELETE /api/v1/agent/stacks/{id}
///
/// Tears the stack down on the docker host first, then drops the row.
/// Mirrors [`api::compose::remove`] without the operator's
/// confirmation password — federation tokens are already a
/// privilege-grant, so requiring a per-call confirmation would just
/// jam primary-side automation.
pub async fn delete_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        assert_owned(&db, &id, &ctx.token_id)?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )?
    };

    let _ = docker::compose::down_stack(&name, &state.config).await;
    let _ = docker::compose::remove_stack(&name, &state.config).await;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute("DELETE FROM services WHERE id = ?1", [&id])?;
    Ok(Json(serde_json::json!({ "ok": true, "action": "deleted" })))
}

/// POST /api/v1/agent/stacks/{id}/deploy
pub async fn deploy_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (name, yaml) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        assert_owned(&db, &id, &ctx.token_id)?;
        db.query_row(
            "SELECT name, compose_content FROM services \
             WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )?
    };
    let yaml = yaml.ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te("errors.federation_agent.stack_no_yaml"))
    })?;

    // Reuse the same auth_map lookup that `api::compose::deploy` does, so
    // private-registry pulls work identically when triggered remotely.
    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &id).ok())
        .unwrap_or_default();
    let auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };

    let output = docker::deploy_service_stack(&state, &id, &name, &yaml, auth).await?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'running', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );

    Ok(Json(serde_json::json!({ "ok": true, "output": output })))
}

/// POST /api/v1/agent/stacks/{id}/down
pub async fn down_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        assert_owned(&db, &id, &ctx.token_id)?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )?
    };
    let output = docker::compose::down_stack(&name, &state.config).await?;
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'stopped', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );
    Ok(Json(serde_json::json!({ "ok": true, "output": output })))
}

/// POST /api/v1/agent/stacks/{id}/restart
///
/// Plain bounce — down then deploy. Heavier than `docker restart
/// <container>` per-container would be (it tears the network down and
/// pulls images again if missing), but matches what the docker-compose
/// CLI does for `compose restart`. Good enough for MVP; finer-grained
/// container restarts are a v2 nice-to-have.
pub async fn restart_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let (name, yaml) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        assert_owned(&db, &id, &ctx.token_id)?;
        db.query_row(
            "SELECT name, compose_content FROM services \
             WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )?
    };
    let yaml = yaml.ok_or_else(|| {
        AppError::BadRequest(crate::i18n::te("errors.federation_agent.stack_no_yaml"))
    })?;

    let _ = docker::compose::down_stack(&name, &state.config).await;

    let auth_map = state
        .db
        .lock()
        .ok()
        .and_then(|db| docker::auth::auth_map_for_service(&db, &id).ok())
        .unwrap_or_default();
    let auth = if auth_map.is_empty() {
        None
    } else {
        Some(auth_map)
    };
    let output = docker::deploy_service_stack(&state, &id, &name, &yaml, auth).await?;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let _ = db.execute(
        "UPDATE services SET status = 'running', updated_at = datetime('now') WHERE id = ?1",
        [&id],
    );
    Ok(Json(serde_json::json!({ "ok": true, "output": output })))
}

#[derive(Deserialize)]
pub struct StackLogsParams {
    /// Tail length; capped at 5000 inside `get_stack_logs`.
    #[serde(default = "default_tail")]
    pub tail: u64,
}

fn default_tail() -> u64 {
    200
}

/// GET /api/v1/agent/stacks/{id}/logs?tail=N
///
/// Snapshot of the last N lines of `docker compose logs`. Returns
/// `text/plain` so the operator's primary UI can dump it verbatim
/// into a `<pre>` without doing JSON-string-escape gymnastics.
pub async fn stack_logs(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
    Query(params): Query<StackLogsParams>,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        assert_owned(&db, &id, &ctx.token_id)?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )?
    };
    let body = crate::docker::compose::get_stack_logs(&name, &state.config, params.tail).await?;
    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    ))
}

/// GET /api/v1/agent/stacks/{id}/logs/ws
///
/// WebSocket — streams `docker compose logs -f` line-by-line for the
/// stack. Auth happens via the federation middleware as usual; browsers
/// can't easily set custom headers on `new WebSocket(...)`, so the
/// middleware also accepts `?token=<plaintext>` as a fallback (the
/// proxy on the primary side passes it through that way).
pub async fn stack_logs_ws(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> AppResult<impl IntoResponse> {
    let name = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        assert_owned(&db, &id, &ctx.token_id)?;
        db.query_row(
            "SELECT name FROM services WHERE id = ?1 AND service_type = 'compose'",
            [&id],
            |row| row.get::<_, String>(0),
        )?
    };
    let config = state.config.clone();
    Ok(ws.on_upgrade(move |socket| async move {
        crate::docker::compose::stream_stack_logs_ws(&name, &config, socket).await;
    }))
}

/// POST /api/v1/agent/rotate-token
///
/// Mints a fresh secret for the **current** federation_tokens row (the
/// one that authenticated this call), persists its hash + prefix, and
/// returns the new plaintext exactly once. Primary uses this in its
/// scheduled rotator to roll federation credentials without operator
/// intervention; identity of the row stays the same so every
/// `owner_server_id` reference keeps pointing at the right primary.
///
/// We do NOT invalidate the old hash here — the caller (primary) is
/// presenting that very hash to authenticate the call. The next call
/// the primary makes will arrive with the new plaintext, hash mismatch
/// on the old one will produce 401 if it's ever re-used, and that's
/// the rotation cliff. Refusing the same in-flight call would race
/// with primary's "store the new token" step.
pub async fn rotate_token(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
) -> AppResult<impl IntoResponse> {
    let issued = crate::auth::federation::generate();
    let new_hash = crate::auth::federation::hash(&issued.plaintext);
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "UPDATE federation_tokens \
         SET token_hash = ?1, token_prefix = ?2 \
         WHERE id = ?3",
        rusqlite::params![new_hash, issued.prefix, ctx.token_id],
    )?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "token": issued.plaintext,
        "token_prefix": issued.prefix,
    })))
}

/// POST /api/v1/agent/release/{stack_id}
///
/// Returns the stack to the peer's own UI. Doesn't touch the running
/// containers — the docker side is untouched, only the DB pointer is
/// reset. The peer's local user can immediately resume management.
pub async fn release_stack(
    State(state): State<SharedState>,
    Extension(ctx): Extension<FederationContext>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    assert_owned(&db, &id, &ctx.token_id)?;
    db.execute(
        "UPDATE services SET owner_server_id = NULL, updated_at = datetime('now') \
         WHERE id = ?1",
        [&id],
    )?;
    Ok(Json(
        serde_json::json!({ "ok": true, "action": "released" }),
    ))
}

/// Ownership guard. Returns:
/// - `Ok(())` when the row exists and is owned by the current token.
/// - `404` when the row doesn't exist (federation must not leak peer-
///   local stack IDs even by way of differing error codes).
/// - `409` when the row is owned by a different primary, or is
///   locally owned by the peer's user.
///
/// Note: we deliberately don't auto-claim unowned rows. The federation
/// token grants the primary the right to *create* and manage *its own*
/// resources on this peer, not to seize whatever already happens to
/// exist here.
fn assert_owned(db: &rusqlite::Connection, stack_id: &str, our_token_id: &str) -> AppResult<()> {
    let owner: Option<Option<String>> = db
        .query_row(
            "SELECT owner_server_id FROM services \
             WHERE id = ?1 AND service_type = 'compose'",
            [stack_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(owner) = owner else {
        return Err(AppError::NotFound(crate::i18n::te_args(
            "errors.federation_agent.stack_not_found",
            &[("v", stack_id)],
        )));
    };
    match owner {
        Some(o) if o == our_token_id => Ok(()),
        Some(_) => Err(AppError::Conflict(crate::i18n::te(
            "errors.federation_agent.stack_owned_by_other_primary",
        ))),
        None => Err(AppError::Conflict(crate::i18n::te(
            "errors.federation_agent.stack_locally_owned",
        ))),
    }
}
