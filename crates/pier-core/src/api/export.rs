//! Emergency export endpoints: build an encrypted zip with env vars (+ local
//! DB dumps) and stream it as a download. See `crate::export`.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use crate::auth::audit::{self, AuthEvent};
use crate::auth::middleware::AuthUser;
use crate::auth::rbac::{enforce_resource_role, ProjectRole};
use crate::error::{AppError, AppResult};
use crate::export::{self, Scope};
use crate::state::SharedState;

const MIN_ARCHIVE_PASSWORD: usize = 8;

#[derive(Deserialize)]
pub struct ExportRequest {
    /// Account password, required when `security.require_password_on_delete` is on.
    #[serde(default)]
    pub password: Option<String>,
    /// Password the zip is encrypted with (AES-256).
    pub archive_password: String,
}

/// POST /api/v1/resources/{id}/export — env (+ DB dump if local) of one service.
/// Editor+, same as reading env.
pub async fn export_resource(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ExportRequest>,
) -> AppResult<Response> {
    enforce_resource_role(&state, &user, &id, ProjectRole::Editor)?;
    run(state, user, Scope::Service(id), body).await
}

/// POST /api/v1/servers/{id}/export — every service on the server. Admin only.
pub async fn export_server(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
    Json(body): Json<ExportRequest>,
) -> AppResult<Response> {
    run(state, user, Scope::Server(id), body).await
}

async fn run(
    state: SharedState,
    user: AuthUser,
    scope: Scope,
    body: ExportRequest,
) -> AppResult<Response> {
    {
        let db = state
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock: {e}")))?;
        crate::api::security::verify_delete_password(&db, &user.id, body.password.as_deref())?;
    }
    if body.archive_password.chars().count() < MIN_ARCHIVE_PASSWORD {
        return Err(AppError::BadRequest(crate::i18n::te_args(
            "errors.export.archive_password_short",
            &[("n", &MIN_ARCHIVE_PASSWORD.to_string())],
        )));
    }

    let (kind, target_id) = match &scope {
        Scope::Service(id) => ("service", id.clone()),
        Scope::Server(id) => ("server", id.clone()),
    };
    let Some((label, services)) = export::load_services(&state, &scope)? else {
        return Err(AppError::NotFound(crate::i18n::te(
            "errors.export.target_not_found",
        )));
    };

    let (file, summary) = export::build_archive(
        &state,
        &format!("{kind}:{label}"),
        &user.username,
        services,
        body.archive_password,
    )
    .await?;

    audit::log(
        &state,
        AuthEvent::SecretsExported,
        Some(&user.id),
        None,
        None,
        Some(serde_json::json!({
            "scope": kind,
            "target_id": target_id,
            "services": summary.services,
            "dumps": summary.dumps,
        })),
    );

    let size = file.metadata().map_err(anyhow::Error::from)?.len();
    let filename = format!(
        "pier-export-{kind}-{}-{}.zip",
        filename_slug(&label),
        chrono::Utc::now().format("%Y%m%d-%H%M%S")
    );
    let body = Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file)));
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/zip".to_string()),
            (CONTENT_LENGTH, size.to_string()),
            (
                CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
            (CACHE_CONTROL, "no-store".to_string()),
        ],
        body,
    )
        .into_response())
}

/// ASCII-only so the Content-Disposition header stays valid.
fn filename_slug(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    match slug.trim_matches('-') {
        "" => "export".into(),
        t => t.to_string(),
    }
}
