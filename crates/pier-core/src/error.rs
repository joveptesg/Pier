use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Unauthorized")]
    Unauthorized,

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Resource name '{name}' already exists")]
    ResourceNameConflict { name: String, existing_id: String },

    #[error("Service has {} domain(s) — confirmation required", .domains.len())]
    #[allow(dead_code)] // Retained for back-compat after Coolify-style refactor.
    DomainsRequireConfirmation { domains: Vec<String> },

    #[error("Docker error: {0}")]
    Docker(#[from] bollard::errors::Error),

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if let Self::ResourceNameConflict { name, existing_id } = &self {
            return (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({
                    "error": format!("Resource '{name}' already exists"),
                    "code": "name_conflict",
                    "name": name,
                    "existing_id": existing_id,
                })),
            )
                .into_response();
        }

        if let Self::DomainsRequireConfirmation { domains } = &self {
            return (
                StatusCode::CONFLICT,
                axum::Json(serde_json::json!({
                    "error": "Disabling public access will remove existing domains",
                    "code": "domains_require_confirmation",
                    "domains": domains,
                })),
            )
                .into_response();
        }

        let (status, message) = match &self {
            Self::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Unauthorized".into()),
            Self::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            Self::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            Self::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            Self::ResourceNameConflict { .. } => unreachable!(),
            Self::DomainsRequireConfirmation { .. } => unreachable!(),
            Self::Docker(e) => {
                tracing::error!("Docker error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Docker error".into())
            }
            Self::Database(e) => {
                tracing::error!("Database error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Database error".into())
            }
            Self::Internal(e) => {
                tracing::error!("Internal error: {e}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error".into())
            }
        };

        (status, axum::Json(serde_json::json!({"error": message}))).into_response()
    }
}

/// Result type alias for handlers.
pub type AppResult<T> = Result<T, AppError>;
