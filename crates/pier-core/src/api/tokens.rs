//! API endpoints for managing the caller's Bearer API tokens.
//!
//! Routes are mounted under `/api/v1/account/tokens` so they piggy-back on
//! the existing session-cookie middleware: a logged-in admin uses these to
//! mint tokens for the npm registry / CI / external integrations.

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::Deserialize;

use crate::auth::api_token;
use crate::auth::middleware::AuthUser;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct CreateTokenRequest {
    pub name: String,
}

/// `POST /api/v1/account/tokens` — issue a new token. The plaintext appears
/// in the response *exactly once* and is never recoverable; clients that
/// lose it have to revoke and reissue.
pub async fn create(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Json(body): Json<CreateTokenRequest>,
) -> AppResult<impl IntoResponse> {
    let name = body.name.trim();
    if name.is_empty() {
        return Err(AppError::BadRequest("name is required".into()));
    }
    if name.len() > 100 {
        return Err(AppError::BadRequest("name too long (max 100)".into()));
    }

    let issued = api_token::generate();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        api_token::store(&db, &issued, &user.id, name)?;
    }

    Ok(Json(serde_json::json!({
        "id": issued.id,
        "name": name,
        "prefix": issued.prefix,
        "token": issued.plaintext,
    })))
}

/// `GET /api/v1/account/tokens` — list active tokens for the current user.
/// Plaintexts never leave the DB; only the prefix is shown.
pub async fn list(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
) -> AppResult<impl IntoResponse> {
    let tokens = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        api_token::list_for_user(&db, &user.id)?
    };
    let body: Vec<_> = tokens
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "id": t.id,
                "name": t.name,
                "prefix": t.prefix,
                "created_at": t.created_at,
                "last_used_at": t.last_used_at,
            })
        })
        .collect();
    Ok(Json(body))
}

/// `DELETE /api/v1/account/tokens/{id}` — revoke a token. Idempotent.
pub async fn revoke(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        api_token::revoke(&db, &id, &user.id).map_err(|e| {
            // The only "not found" case is from `revoke()`; treat as 404.
            if e.to_string().contains("not found") {
                AppError::NotFound(format!("token {id}"))
            } else {
                AppError::Internal(e)
            }
        })?;
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}
