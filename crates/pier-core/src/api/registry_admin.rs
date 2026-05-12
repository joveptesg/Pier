//! Panel-side mutation endpoints for the embedded npm registry.
//!
//! Mirrors the npm-protocol routes (dist-tag, unpublish, deprecate) but
//! authenticated via the regular session cookie instead of a Bearer token —
//! so the package detail page can wire its buttons up via plain `fetch()` from
//! the browser without round-tripping through `npm` CLI semantics.
//!
//! Permission policy is identical to the npm-protocol side: the original
//! publisher (or an `admin`) can mutate; anyone else gets 401.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use serde::Deserialize;
use std::collections::BTreeMap;

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::registry::{self, db as regdb, storage};
use crate::state::SharedState;

#[derive(Debug, Deserialize)]
pub struct SetDistTagBody {
    pub version: String,
}

#[derive(Debug, Deserialize)]
pub struct DeprecateBody {
    /// Empty string clears the deprecation flag, matching `npm deprecate <pkg> ""`.
    #[serde(default)]
    pub message: String,
}

pub async fn set_dist_tag(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, tag)): Path<(String, String)>,
    Json(body): Json<SetDistTagBody>,
) -> Result<impl IntoResponse, AppError> {
    crate::api::npm::validate_package_name(&package)?;
    require_can_modify(&state, &user, &package, Some(&body.version)).await?;
    let pkg = package.clone();
    let ver = body.version.clone();
    let tag_db = tag.clone();
    db_blocking(&state, move |db| {
        regdb::set_dist_tag(db, &pkg, &tag_db, &ver)
    })
    .await
    .map_err(map_db_user_error)?;
    tracing::info!(
        "panel: set dist-tag {package}@{tag} -> {} by {}",
        body.version,
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

pub async fn remove_dist_tag(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, tag)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    crate::api::npm::validate_package_name(&package)?;
    require_can_modify(&state, &user, &package, None).await?;
    let pkg = package.clone();
    let tag_db = tag.clone();
    let removed = db_blocking(&state, move |db| regdb::remove_dist_tag(db, &pkg, &tag_db))
        .await
        .map_err(map_db_user_error)?;
    if !removed {
        return Err(AppError::NotFound(format!("dist-tag {tag}")));
    }
    tracing::info!(
        "panel: removed dist-tag {package}@{tag} by {}",
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

pub async fn deprecate_version(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, version)): Path<(String, String)>,
    Json(body): Json<DeprecateBody>,
) -> Result<impl IntoResponse, AppError> {
    crate::api::npm::validate_package_name(&package)?;
    require_can_modify(&state, &user, &package, Some(&version)).await?;
    let pkg = package.clone();
    let mut messages: BTreeMap<String, String> = BTreeMap::new();
    messages.insert(version.clone(), body.message.clone());
    db_blocking(&state, move |db| {
        regdb::deprecate_versions(db, &pkg, &messages)
    })
    .await?;
    let action = if body.message.is_empty() {
        "un-deprecated"
    } else {
        "deprecated"
    };
    tracing::info!("panel: {action} {package}@{version} by {}", user.username);
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

pub async fn delete_version(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, version)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    crate::api::npm::validate_package_name(&package)?;
    require_can_modify(&state, &user, &package, Some(&version)).await?;
    let pkg = package.clone();
    let ver = version.clone();
    let removed = db_blocking(&state, move |db| regdb::delete_version(db, &pkg, &ver)).await?;
    let Some(removed) = removed else {
        return Err(AppError::NotFound(format!("{package}@{version}")));
    };
    let _ = storage::delete_tarball(&state, &removed.package, &removed.filename).await;
    tracing::info!(
        "panel: unpublished {package}@{version} by {}",
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

pub async fn delete_package(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path(package): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    crate::api::npm::validate_package_name(&package)?;
    require_can_modify(&state, &user, &package, None).await?;
    let pkg = package.clone();
    let removed = db_blocking(&state, move |db| regdb::delete_package(db, &pkg)).await?;
    if removed.is_empty() {
        return Err(AppError::NotFound(format!("package {package}")));
    }
    for r in &removed {
        let _ = storage::delete_tarball(&state, &r.package, &r.filename).await;
    }
    tracing::info!(
        "panel: unpublished {package} ({} versions) by {}",
        removed.len(),
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))))
}

// ----- shared helpers (kept local — npm.rs counterparts aren't pub) ---------

async fn db_blocking<T, F>(state: &SharedState, f: F) -> Result<T, AppError>
where
    F: FnOnce(&rusqlite::Connection) -> anyhow::Result<T> + Send + 'static,
    T: Send + 'static,
{
    let state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<T> {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock poisoned: {e}"))?;
        f(&db)
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))?;
    result.map_err(AppError::Internal)
}

fn map_db_user_error(err: AppError) -> AppError {
    let AppError::Internal(e) = err else {
        return err;
    };
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("invalid dist-tag")
        || lower.contains("does not exist")
        || lower.contains("refusing to remove")
        || lower.contains("package not found")
    {
        AppError::BadRequest(msg)
    } else {
        AppError::Internal(e)
    }
}

async fn require_can_modify(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
    version: Option<&str>,
) -> Result<(), AppError> {
    if user.role == "admin" {
        return Ok(());
    }
    let package_owned = package.to_string();
    let version_owned = version.map(|v| v.to_string());
    let user_id = user.id.clone();
    let ok = db_blocking(state, move |db| {
        if let Some(v) = &version_owned {
            let row: Option<Option<String>> = db
                .query_row(
                    "SELECT published_by FROM npm_versions
                     WHERE package_name = ?1 AND version = ?2",
                    rusqlite::params![&package_owned, v],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok();
            match row {
                None => Ok(false),
                Some(None) => Ok(true),
                Some(Some(uid)) => Ok(uid == user_id),
            }
        } else {
            let owns: bool = db
                .query_row(
                    "SELECT 1 FROM npm_versions
                     WHERE package_name = ?1 AND published_by = ?2 LIMIT 1",
                    rusqlite::params![&package_owned, &user_id],
                    |_| Ok(true),
                )
                .ok()
                .is_some();
            let any_tracked: bool = db
                .query_row(
                    "SELECT 1 FROM npm_versions
                     WHERE package_name = ?1 AND published_by IS NOT NULL LIMIT 1",
                    rusqlite::params![&package_owned],
                    |_| Ok(true),
                )
                .ok()
                .is_some();
            Ok(owns || !any_tracked)
        }
    })
    .await?;
    if ok {
        Ok(())
    } else {
        Err(AppError::Unauthorized)
    }
}

// Wire `registry::tarball_filename` so rustfmt doesn't flag the import as
// unused if a future refactor drops it from the handlers above.
const _: fn(&str, &str) -> String = registry::tarball_filename;
