use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;

use crate::docker;
use crate::error::AppResult;
use crate::state::SharedState;

/// GET /api/v1/images
pub async fn list(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let images = docker::images::list_images(&state.docker).await?;
    Ok(Json(images))
}

/// DELETE /api/v1/images/:id
pub async fn remove(
    State(state): State<SharedState>,
    Path(id): Path<String>,
) -> AppResult<impl IntoResponse> {
    docker::images::remove_image(&state.docker, &id, false).await?;
    Ok(Json(serde_json::json!({"ok": true, "action": "removed"})))
}
