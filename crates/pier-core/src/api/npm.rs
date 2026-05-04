//! npm-compatible registry HTTP handlers.
//!
//! Mounted at `/registry/npm/` so the registry URL clients configure is
//! `https://<pier-host>/registry/npm/`. Routes follow the npm registry API
//! described in [REGISTRY-API.md](https://github.com/npm/registry/blob/main/docs/REGISTRY-API.md):
//!
//! - `GET  /-/ping`                        — health probe
//! - `GET  /-/whoami`                      — returns the bearer's username
//! - `PUT  /-/user/org.couchdb.user:{name}`— `npm login` (issues an api_token)
//! - `GET  /{package}`                     — packument
//! - `GET  /{package}/{version}`           — single-version manifest
//! - `GET  /{package}/-/{tarball}`         — tarball bytes
//! - `PUT  /{package}`                     — `npm publish`
//!
//! Scoped variants of every package route are mounted in parallel (`/@{scope}/{name}/...`).

use std::collections::BTreeMap;

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use base64::Engine;
use serde::Deserialize;

use crate::auth::api_token;
use crate::auth::middleware::{require_auth, AuthUser};
use crate::auth::password;
use crate::error::AppError;
use crate::registry::{self, db as regdb, storage};
use crate::state::SharedState;

/// Build the registry router mounted at `/registry/npm`.
pub fn router(state: SharedState) -> Router<SharedState> {
    let public = Router::new()
        .route("/-/ping", get(ping))
        .route("/-/user/{*name}", put(login));

    let protected = Router::new()
        .route("/-/whoami", get(whoami))
        // Scoped variants come first so axum tries the more specific match.
        .route("/@{scope}/{name}/-/{tarball}", get(get_tarball_scoped))
        .route("/@{scope}/{name}/{version}", get(get_version_scoped))
        .route(
            "/@{scope}/{name}",
            get(get_packument_scoped).put(publish_scoped),
        )
        .route("/{package}/-/{tarball}", get(get_tarball_unscoped))
        .route("/{package}/{version}", get(get_version_unscoped))
        .route(
            "/{package}",
            get(get_packument_unscoped).put(publish_unscoped),
        )
        .layer(axum::middleware::from_fn_with_state(state, require_auth));

    public.merge(protected)
}

// ----- public endpoints -------------------------------------------------------

async fn ping() -> Json<serde_json::Value> {
    Json(serde_json::json!({}))
}

#[derive(Deserialize)]
struct LoginBody {
    name: Option<String>,
    password: Option<String>,
}

/// `npm login` (legacy CouchDB-compatible). Validates the username/password
/// against the `users` table and issues a fresh api_token tied to that user.
async fn login(
    State(state): State<SharedState>,
    Path(_couch_user_path): Path<String>,
    Json(body): Json<LoginBody>,
) -> Result<impl IntoResponse, AppError> {
    let username = body
        .name
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("missing name".into()))?
        .trim();
    let plain = body
        .password
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("missing password".into()))?;

    let issued = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let user = db
            .query_row(
                "SELECT id, password FROM users
                 WHERE (username = ?1 OR email = ?1) AND is_active = 1",
                [username],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| AppError::Unauthorized)?;

        if !password::verify_password(plain, &user.1)? {
            return Err(AppError::Unauthorized);
        }

        let token = api_token::generate();
        api_token::store(&db, &token, &user.0, "npm login")?;
        token
    };

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "id": format!("org.couchdb.user:{username}"),
            "token": issued.plaintext,
        })),
    ))
}

// ----- protected endpoints ----------------------------------------------------

async fn whoami(Extension(user): Extension<AuthUser>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "username": user.username }))
}

async fn get_packument_unscoped(
    State(state): State<SharedState>,
    Path(package): Path<String>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    serve_packument(&state, &headers, &package).await
}

async fn get_packument_scoped(
    State(state): State<SharedState>,
    Path((scope, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    serve_packument(&state, &headers, &pkg).await
}

async fn get_version_unscoped(
    State(state): State<SharedState>,
    Path((package, version)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    serve_version(&state, &headers, &package, &version).await
}

async fn get_version_scoped(
    State(state): State<SharedState>,
    Path((scope, name, version)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    serve_version(&state, &headers, &pkg, &version).await
}

async fn get_tarball_unscoped(
    State(state): State<SharedState>,
    Path((package, tarball)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    serve_tarball(&state, &package, &tarball).await
}

async fn get_tarball_scoped(
    State(state): State<SharedState>,
    Path((scope, name, tarball)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    serve_tarball(&state, &pkg, &tarball).await
}

async fn publish_unscoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path(package): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    handle_publish(&state, &user, &headers, &package, &body).await
}

async fn publish_scoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((scope, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    handle_publish(&state, &user, &headers, &pkg, &body).await
}

// ----- shared implementations -------------------------------------------------

async fn serve_packument(
    state: &SharedState,
    headers: &HeaderMap,
    package: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let packument = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        regdb::load_packument(&db, package)?
    };
    let Some(mut packument) = packument else {
        return Err(AppError::NotFound(format!("package {package}")));
    };

    let base = public_base_url(headers);
    rewrite_tarball_urls(&mut packument, &base);

    let body = serde_json::to_value(packument).map_err(anyhow::Error::from)?;
    Ok(Json(body).into_response())
}

async fn serve_version(
    state: &SharedState,
    headers: &HeaderMap,
    package: &str,
    version: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let manifest = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        regdb::load_version_manifest(&db, package, version)?
    };
    let Some(mut manifest) = manifest else {
        return Err(AppError::NotFound(format!("{package}@{version}")));
    };

    let base = public_base_url(headers);
    rewrite_single_manifest(&mut manifest, package, &base);

    Ok(Json(manifest).into_response())
}

async fn serve_tarball(
    state: &SharedState,
    package: &str,
    tarball: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    if !tarball.ends_with(".tgz") {
        return Err(AppError::BadRequest("tarball must end in .tgz".into()));
    }

    let version = derive_version_from_tarball(package, tarball)
        .ok_or_else(|| AppError::BadRequest("malformed tarball name".into()))?;

    // Validate the version exists in our index — guards against path traversal
    // probes via `..` in the tarball segment.
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        if regdb::lookup_tarball(&db, package, &version)?.is_none() {
            return Err(AppError::NotFound(format!("{package}-{version}")));
        }
    }

    let bytes = storage::read_tarball(state, package, tarball)
        .await
        .map_err(|e| {
            tracing::error!("registry: read_tarball failed: {e:#}");
            AppError::NotFound(format!("{package}/{tarball}"))
        })?;

    Ok((
        StatusCode::OK,
        [(CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response())
}

async fn handle_publish(
    state: &SharedState,
    user: &AuthUser,
    headers: &HeaderMap,
    package: &str,
    body: &[u8],
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let body_json: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("invalid json: {e}")))?;

    // Pull the single (version → manifest) entry.
    let versions_obj = body_json
        .get("versions")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppError::BadRequest("missing versions{}".into()))?;
    if versions_obj.len() != 1 {
        return Err(AppError::BadRequest(
            "publish must contain exactly one version".into(),
        ));
    }
    let (version, manifest_val) = versions_obj
        .iter()
        .next()
        .ok_or_else(|| AppError::BadRequest("empty versions{}".into()))?;
    let version = version.clone();

    let description = body_json
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    // Pull the single attachment entry.
    let attachments = body_json
        .get("_attachments")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppError::BadRequest("missing _attachments{}".into()))?;
    if attachments.is_empty() {
        return Err(AppError::BadRequest("no _attachments".into()));
    }
    let (attachment_name, attachment) = attachments
        .iter()
        .next()
        .ok_or_else(|| AppError::BadRequest("empty _attachments".into()))?;
    let data_b64 = attachment
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("attachment missing data".into()))?;
    let tarball_bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| AppError::BadRequest(format!("base64 decode: {e}")))?;

    // Sanity-check the filename (prevents `_attachments['../../foo']` paths).
    let expected_name = registry::tarball_filename(package, &version);
    if attachment_name != &expected_name {
        return Err(AppError::BadRequest(format!(
            "attachment name '{attachment_name}' does not match expected '{expected_name}'"
        )));
    }

    // Verify integrity if the client supplied one.
    if let Some(claimed) = manifest_val
        .get("dist")
        .and_then(|d| d.get("integrity"))
        .and_then(|v| v.as_str())
    {
        let computed = storage::integrity(&tarball_bytes);
        if claimed != computed {
            return Err(AppError::BadRequest(
                "tarball integrity does not match dist.integrity".into(),
            ));
        }
    }

    let tarball_size = tarball_bytes.len() as i64;
    let computed_integrity = storage::integrity(&tarball_bytes);

    // Inject canonical `dist` fields into the manifest before persisting.
    let mut manifest_owned = manifest_val.clone();
    let base = public_base_url(headers);
    let tarball_url = build_tarball_url(&base, package, &expected_name);
    {
        let dist = manifest_owned
            .get_mut("dist")
            .and_then(|v| v.as_object_mut())
            .map(|m| m as &mut serde_json::Map<String, serde_json::Value>);
        if let Some(dist) = dist {
            dist.insert(
                "tarball".into(),
                serde_json::Value::String(tarball_url.clone()),
            );
            dist.insert(
                "integrity".into(),
                serde_json::Value::String(computed_integrity.clone()),
            );
            dist.insert("size".into(), serde_json::Value::from(tarball_size));
        } else {
            // No `dist` provided — synthesise it.
            let mut m = serde_json::Map::new();
            m.insert(
                "tarball".into(),
                serde_json::Value::String(tarball_url.clone()),
            );
            m.insert(
                "integrity".into(),
                serde_json::Value::String(computed_integrity.clone()),
            );
            m.insert("size".into(), serde_json::Value::from(tarball_size));
            manifest_owned
                .as_object_mut()
                .ok_or_else(|| AppError::BadRequest("manifest is not an object".into()))?
                .insert("dist".into(), serde_json::Value::Object(m));
        }
    }
    let manifest_json = serde_json::to_string(&manifest_owned).map_err(anyhow::Error::from)?;

    // Merge new dist-tags into existing ones (defaulting to `latest`).
    let new_tags = body_json
        .get("dist-tags")
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect::<BTreeMap<String, String>>()
        })
        .unwrap_or_else(|| {
            let mut m = BTreeMap::new();
            m.insert("latest".into(), version.clone());
            m
        });

    // Write tarball BEFORE inserting DB row — gc_orphans will reap an
    // orphaned blob if the DB insert subsequently fails.
    storage::write_tarball(state, package, &expected_name, tarball_bytes).await?;

    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        // Merge dist-tags with what's already in the DB so we never drop tags.
        let mut merged: BTreeMap<String, String> = db
            .query_row(
                "SELECT dist_tags_json FROM npm_packages WHERE name = ?1",
                [package],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        for (k, v) in new_tags {
            merged.insert(k, v);
        }

        match regdb::insert_version(
            &db,
            package,
            &description,
            &version,
            &manifest_json,
            tarball_size,
            &computed_integrity,
            Some(&user.id),
            &merged,
        ) {
            Ok(()) => {}
            Err(e) => {
                // UNIQUE-violation → 409 (re-publish of an existing version).
                let msg = e.to_string();
                if msg.contains("UNIQUE") {
                    return Err(AppError::Conflict(format!(
                        "{package}@{version} already published"
                    )));
                }
                return Err(AppError::Internal(e));
            }
        }
    }

    tracing::info!(
        "registry: published {package}@{version} ({tarball_size} bytes) by {user}",
        user = user.username
    );

    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "ok": true,
            "id": package,
            "rev": format!("1-{}", &computed_integrity[..16.min(computed_integrity.len())]),
        })),
    )
        .into_response())
}

// ----- helpers ----------------------------------------------------------------

/// Reject obviously bad package names early (path traversal, empty, etc).
/// The full npm validity rules live in `validate-npm-package-name` —
/// we only enforce the security-critical subset here.
fn validate_package_name(name: &str) -> Result<(), AppError> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('\\')
        || name.starts_with('-')
        || name.starts_with('.')
        || name.len() > 214
    {
        return Err(AppError::BadRequest("invalid package name".into()));
    }
    if let Some(rest) = name.strip_prefix('@') {
        // Scoped: must be `@scope/name`.
        let Some((scope, n)) = rest.split_once('/') else {
            return Err(AppError::BadRequest("scoped package missing /name".into()));
        };
        if scope.is_empty() || n.is_empty() || n.contains('/') || scope.contains('/') {
            return Err(AppError::BadRequest("invalid scoped package name".into()));
        }
    } else if name.contains('/') {
        return Err(AppError::BadRequest(
            "unscoped package cannot contain /".into(),
        ));
    }
    Ok(())
}

/// Derive `version` from `{name}-{version}.tgz` (or `{name}-{version}.tgz` for
/// scoped, where the filename uses just the unscoped component).
fn derive_version_from_tarball(package: &str, tarball: &str) -> Option<String> {
    let stem = tarball.strip_suffix(".tgz")?;
    let unscoped = package.rsplit_once('/').map(|(_, n)| n).unwrap_or(package);
    let prefix = format!("{unscoped}-");
    stem.strip_prefix(&prefix).map(|s| s.to_string())
}

/// Detect the public scheme/host so `dist.tarball` URLs round-trip back to us.
fn public_base_url(headers: &HeaderMap) -> String {
    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .unwrap_or_else(|| "http".to_string());
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(HOST))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    format!("{scheme}://{host}")
}

fn build_tarball_url(base: &str, package: &str, filename: &str) -> String {
    format!("{base}/registry/npm/{package}/-/{filename}")
}

fn rewrite_tarball_urls(packument: &mut regdb::Packument, base: &str) {
    let pkg_name = packument.name.clone();
    for (_, manifest) in packument.versions.iter_mut() {
        rewrite_single_manifest(manifest, &pkg_name, base);
    }
}

fn rewrite_single_manifest(manifest: &mut serde_json::Value, package: &str, base: &str) {
    // Snapshot the version up front so we can fall back to a synthesised
    // tarball filename even after we take a mutable borrow on `dist`.
    let version_fallback = manifest
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

    let dist = match manifest.get_mut("dist").and_then(|v| v.as_object_mut()) {
        Some(d) => d,
        None => return,
    };
    let filename = dist
        .get("tarball")
        .and_then(|v| v.as_str())
        .and_then(|url| url.rsplit('/').next())
        .map(|s| s.to_string())
        .unwrap_or_else(|| registry::tarball_filename(package, &version_fallback));
    dist.insert(
        "tarball".into(),
        serde_json::Value::String(build_tarball_url(base, package, &filename)),
    );
}

// Suppress an `unused_imports` warning for `AUTHORIZATION` — the import is
// here so middleware-level changes that need to inspect this header in the
// future can do so without re-importing. Not used in the current file.
#[allow(dead_code)]
const _: Option<axum::http::HeaderName> = Some(AUTHORIZATION);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_traversal() {
        assert!(validate_package_name("../etc").is_err());
        assert!(validate_package_name("foo/../bar").is_err());
        assert!(validate_package_name("").is_err());
    }

    #[test]
    fn validate_accepts_scoped() {
        assert!(validate_package_name("@scope/name").is_ok());
        assert!(validate_package_name("react").is_ok());
    }

    #[test]
    fn version_extraction() {
        assert_eq!(
            derive_version_from_tarball("react", "react-18.2.0.tgz"),
            Some("18.2.0".into())
        );
        assert_eq!(
            derive_version_from_tarball("@scope/name", "name-1.0.0.tgz"),
            Some("1.0.0".into())
        );
        assert_eq!(
            derive_version_from_tarball("react", "other-1.0.0.tgz"),
            None
        );
    }
}
