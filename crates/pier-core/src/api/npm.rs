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
//! - `GET  /{package}/-/{tarball}`         — tarball bytes (streamed)
//! - `PUT  /{package}`                     — `npm publish`
//!
//! Scoped variants of every package route are mounted in parallel (`/@{scope}/{name}/...`).
//!
//! Concurrency: every handler that touches SQLite hops onto a blocking thread
//! via `spawn_blocking`, because `state.db` is a `std::sync::Mutex<Connection>`
//! and holding it across `.await` would stall the entire tokio runtime under
//! load.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, ETAG, HOST, IF_NONE_MATCH,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, put};
use axum::{Extension, Json, Router};
use base64::Engine;
use rusqlite::Connection;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio_util::io::ReaderStream;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::GovernorLayer;
use tower_http::compression::CompressionLayer;

use crate::auth::api_token;
use crate::auth::middleware::{require_auth, AuthUser};
use crate::auth::password;
use crate::error::AppError;
use crate::registry::{self, db as regdb, storage, upstream};
use crate::state::SharedState;

/// Per-publish body cap. 100 MiB is large enough for monorepo-sized tarballs
/// (the npm.org-public p99 is ~5 MiB; popular monorepo packages reach tens of
/// MiB) while keeping a malicious client from filling RAM. Tunable later via
/// a settings row if real workloads need more.
const PUBLISH_BODY_LIMIT: usize = 100 * 1024 * 1024;

/// Accept-header media type that requests the abbreviated packument format
/// described in [registry/responses/package-metadata.md](https://github.com/npm/registry/blob/main/docs/responses/package-metadata.md).
/// npm 7+, yarn berry, pnpm and bun all send this header by default and the
/// payload is ×5-×10 smaller than a full packument for monorepo packages.
const ABBREVIATED_MEDIA_TYPE: &str = "application/vnd.npm.install-v1+json";

/// Manifest fields kept in the abbreviated packument. Everything else
/// (`readme`, `author`, `contributors`, `keywords`, `scripts`, …) is dropped.
const ABBREVIATED_KEYS: &[&str] = &[
    "name",
    "version",
    "deprecated",
    "dependencies",
    "optionalDependencies",
    "devDependencies",
    "bundleDependencies",
    "peerDependencies",
    "peerDependenciesMeta",
    "bin",
    "directories",
    "dist",
    "engines",
    "_hasShrinkwrap",
    "hasInstallScript",
];

/// Build the registry router mounted at `/registry/npm`.
///
/// Rate-limit profile (per peer IP):
/// - Read endpoints (packument, version, tarball): 200 req/min burst 50.
/// - Publish (PUT body, expensive): ~10 req/min, burst 3.
/// - Admin mutations (dist-tag / deprecate / unpublish — cheap DB-only ops):
///   30 req/min, burst 10. A real user clicking buttons in the UI or running
///   `npm dist-tag add` on a release stream won't trip this; only an obvious
///   abuse loop will.
/// - Login (PUT /-/user/...): 12 req/min, burst 5 (same as `/auth/login`).
///
/// `ping` and `whoami` are intentionally not rate-limited — clients probe them
/// constantly during `npm install` and they touch nothing but a hash map.
pub fn router(state: SharedState) -> Router<SharedState> {
    let read_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(1)
            .burst_size(50)
            .finish()
            .expect("registry read governor config"),
    );
    let publish_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(6)
            .burst_size(3)
            .finish()
            .expect("registry publish governor config"),
    );
    // Distinct from `publish_governor` because dist-tag / deprecate / unpublish
    // are cheap (single-row UPDATE / DELETE) but a user can legitimately fire
    // several in quick succession — promoting a release, renaming tags, etc.
    let admin_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(2)
            .burst_size(10)
            .finish()
            .expect("registry admin governor config"),
    );
    let login_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(12)
            .burst_size(5)
            .finish()
            .expect("registry login governor config"),
    );

    let public = Router::new()
        .route("/-/ping", get(ping))
        .route(
            "/-/user/{*name}",
            put(login).layer(GovernorLayer::new(login_governor)),
        )
        .merge(crate::api::npm_web_login::public_router());

    // JSON metadata routes — gzip compression saves serious bandwidth on
    // packuments (often hundreds of KB). NOT applied to tarball routes
    // because tarballs are already .tgz-compressed.
    let read_json = Router::new()
        // Scoped variants come first so axum tries the more specific match.
        .route("/@{scope}/{name}/{version}", get(get_version_scoped))
        .route("/@{scope}/{name}", get(get_packument_scoped))
        .route("/{package}/{version}", get(get_version_unscoped))
        .route("/{package}", get(get_packument_unscoped))
        .layer(CompressionLayer::new().gzip(true));

    // Tarball routes — GET streams the blob, HEAD returns metadata only
    // (pnpm and bun probe with HEAD before downloading large tarballs).
    let read_tarball = Router::new()
        .route(
            "/@{scope}/{name}/-/{tarball}",
            get(get_tarball_scoped).head(head_tarball_scoped),
        )
        .route(
            "/{package}/-/{tarball}",
            get(get_tarball_unscoped).head(head_tarball_unscoped),
        );

    let read_routes = read_json
        .merge(read_tarball)
        .layer(GovernorLayer::new(read_governor));

    let publish_routes = Router::new()
        .route("/@{scope}/{name}", put(publish_scoped))
        .route("/{package}", put(publish_unscoped))
        .layer(DefaultBodyLimit::max(PUBLISH_BODY_LIMIT))
        .layer(GovernorLayer::new(publish_governor));

    // Dist-tag management — `npm dist-tag add/rm/ls`. Both scoped and
    // unscoped paths because axum can't tell from `/{pkg}` whether `{pkg}`
    // already includes a slash.
    let dist_tag_routes = Router::new()
        .route(
            "/-/package/{package}/dist-tags",
            get(get_dist_tags_unscoped),
        )
        .route(
            "/-/package/@{scope}/{name}/dist-tags",
            get(get_dist_tags_scoped),
        )
        .route(
            "/-/package/{package}/dist-tags/{tag}",
            put(set_dist_tag_unscoped)
                .post(set_dist_tag_unscoped)
                .delete(remove_dist_tag_unscoped),
        )
        .route(
            "/-/package/@{scope}/{name}/dist-tags/{tag}",
            put(set_dist_tag_scoped)
                .post(set_dist_tag_scoped)
                .delete(remove_dist_tag_scoped),
        )
        .layer(GovernorLayer::new(admin_governor.clone()));

    // Mutation endpoints used by `npm unpublish` and `npm deprecate`.
    // - DELETE /{pkg}/-rev/{rev}              → full unpublish
    // - DELETE /{pkg}/-/{tarball}/-rev/{rev}  → single-version unpublish
    // - PUT    /{pkg}/-rev/{rev}              → deprecate (parse manifest body)
    // {rev} is required by the npm protocol but we don't gate on it.
    let admin_routes = Router::new()
        .route(
            "/{package}/-rev/{rev}",
            delete(delete_package_unscoped).put(deprecate_unscoped),
        )
        .route(
            "/@{scope}/{name}/-rev/{rev}",
            delete(delete_package_scoped).put(deprecate_scoped),
        )
        .route(
            "/{package}/-/{tarball}/-rev/{rev}",
            delete(delete_version_unscoped),
        )
        .route(
            "/@{scope}/{name}/-/{tarball}/-rev/{rev}",
            delete(delete_version_scoped),
        )
        .layer(DefaultBodyLimit::max(PUBLISH_BODY_LIMIT))
        .layer(GovernorLayer::new(admin_governor));

    let protected = Router::new()
        .route("/-/whoami", get(whoami))
        .merge(read_routes)
        .merge(publish_routes)
        .merge(dist_tag_routes)
        .merge(admin_routes)
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
        .trim()
        .to_string();
    let plain = body
        .password
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("missing password".into()))?
        .to_string();

    // The bcrypt verify inside this closure is CPU-heavy (~100ms on a small VPS).
    // Running it on the blocking pool keeps the async runtime free for live
    // installs in flight.
    let state_cl = state.clone();
    let username_cl = username.clone();
    let issued = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock poisoned: {e}")))?;
        let user = db
            .query_row(
                "SELECT id, password FROM users
                 WHERE (username = ?1 OR email = ?1) AND is_active = 1",
                [&username_cl],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|_| AppError::Unauthorized)?;

        if !password::verify_password(&plain, &user.1)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("verify_password: {e}")))?
        {
            return Err(AppError::Unauthorized);
        }

        let token = api_token::generate();
        api_token::store(&db, &token, &user.0, "npm login")
            .map_err(|e| AppError::Internal(anyhow::anyhow!("store token: {e}")))?;
        Ok(token)
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))??;

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
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    serve_tarball(&state, &headers, &package, &tarball).await
}

async fn get_tarball_scoped(
    State(state): State<SharedState>,
    Path((scope, name, tarball)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    serve_tarball(&state, &headers, &pkg, &tarball).await
}

async fn head_tarball_unscoped(
    State(state): State<SharedState>,
    Path((package, tarball)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    serve_tarball_head(&state, &package, &tarball).await
}

async fn head_tarball_scoped(
    State(state): State<SharedState>,
    Path((scope, name, tarball)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    serve_tarball_head(&state, &pkg, &tarball).await
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

// ----- dist-tag handlers (`npm dist-tag ls/add/rm`) -------------------------

async fn get_dist_tags_unscoped(
    State(state): State<SharedState>,
    Path(package): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    serve_dist_tags(&state, &package).await
}

async fn get_dist_tags_scoped(
    State(state): State<SharedState>,
    Path((scope, name)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    serve_dist_tags(&state, &pkg).await
}

async fn set_dist_tag_unscoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, tag)): Path<(String, String)>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    handle_set_dist_tag(&state, &user, &package, &tag, &body).await
}

async fn set_dist_tag_scoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((scope, name, tag)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    handle_set_dist_tag(&state, &user, &pkg, &tag, &body).await
}

async fn remove_dist_tag_unscoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, tag)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    handle_remove_dist_tag(&state, &user, &package, &tag).await
}

async fn remove_dist_tag_scoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((scope, name, tag)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    handle_remove_dist_tag(&state, &user, &pkg, &tag).await
}

// ----- unpublish handlers (`npm unpublish`) ----------------------------------

async fn delete_package_unscoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, _rev)): Path<(String, String)>,
) -> Result<impl IntoResponse, AppError> {
    handle_delete_package(&state, &user, &package).await
}

async fn delete_package_scoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((scope, name, _rev)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    handle_delete_package(&state, &user, &pkg).await
}

async fn delete_version_unscoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, tarball, _rev)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    handle_delete_version(&state, &user, &package, &tarball).await
}

async fn delete_version_scoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((scope, name, tarball, _rev)): Path<(String, String, String, String)>,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    handle_delete_version(&state, &user, &pkg, &tarball).await
}

// ----- deprecate handlers (`npm deprecate`) ---------------------------------

async fn deprecate_unscoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((package, _rev)): Path<(String, String)>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    handle_deprecate(&state, &user, &package, &body).await
}

async fn deprecate_scoped(
    State(state): State<SharedState>,
    Extension(user): Extension<AuthUser>,
    Path((scope, name, _rev)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    let pkg = format!("@{scope}/{name}");
    handle_deprecate(&state, &user, &pkg, &body).await
}

// ----- shared implementations -------------------------------------------------

/// Run a closure that reads from SQLite on the blocking thread pool.
///
/// Holding `state.db.lock()` (std::sync::Mutex) across an `.await` would block
/// other async tasks scheduled on the same worker thread. Wrapping every DB
/// call in `spawn_blocking` keeps the async runtime free for concurrent
/// installs/publishes.
async fn db_blocking<T, F>(state: &SharedState, f: F) -> Result<T, AppError>
where
    F: FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
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

async fn serve_packument(
    state: &SharedState,
    headers: &HeaderMap,
    package: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let package_owned = package.to_string();
    let mut packument =
        db_blocking(state, move |db| regdb::load_packument(db, &package_owned)).await?;

    // Proxy miss-fill: when the local DB has no row for this package and
    // upstream-proxy mode is enabled, fetch the packument from upstream,
    // cache it as is_proxy=1, then reload. A missing-on-upstream result
    // still passes through as 404 to the client. Failures (DNS, network)
    // surface as a 502 — staler-data fallback lands in sub-PR 3 alongside
    // TTL refresh.
    if packument.is_none() {
        let cfg = db_blocking(state, move |db| Ok(upstream::load_config(db))).await?;
        if cfg.enabled {
            tracing::debug!(%package, upstream = %cfg.upstream_url, "registry proxy miss → upstream fetch");
            let want_abbrev = wants_abbreviated(headers);
            let fetched = upstream::fetch_packument(&cfg.upstream_url, package, want_abbrev)
                .await
                .map_err(|e| {
                    tracing::warn!(%package, "upstream fetch failed: {e:#}");
                    // Map to 502-equivalent through Internal — adding a
                    // BadGateway variant is a separate concern (every panel
                    // call site would need to think about it). The handler
                    // log carries the original detail for the operator.
                    AppError::Internal(anyhow::anyhow!("upstream fetch failed: {e}"))
                })?;
            if let Some(up) = fetched {
                let package_owned = package.to_string();
                let body_clone = up.body.clone();
                let etag_clone = up.etag.clone();
                db_blocking(state, move |db| {
                    regdb::upsert_proxy_packument(
                        db,
                        &package_owned,
                        &body_clone,
                        etag_clone.as_deref(),
                    )
                })
                .await?;
                let package_owned = package.to_string();
                packument = db_blocking(state, move |db| {
                    regdb::load_packument(db, &package_owned)
                })
                .await?;
            }
        }
    }

    let Some(mut packument) = packument else {
        return Err(AppError::NotFound(format!("package {package}")));
    };

    let base = public_base_url(headers);
    rewrite_tarball_urls(&mut packument, &base);

    let value = if wants_abbreviated(headers) {
        build_abbreviated_packument(&packument)
    } else {
        serde_json::to_value(&packument).map_err(anyhow::Error::from)?
    };
    json_response(headers, &value)
}

async fn serve_version(
    state: &SharedState,
    headers: &HeaderMap,
    package: &str,
    version: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let package_owned = package.to_string();
    let version_owned = version.to_string();
    let manifest = db_blocking(state, move |db| {
        regdb::load_version_manifest(db, &package_owned, &version_owned)
    })
    .await?;

    let Some(mut manifest) = manifest else {
        return Err(AppError::NotFound(format!("{package}@{version}")));
    };

    let base = public_base_url(headers);
    rewrite_single_manifest(&mut manifest, package, &base);

    json_response(headers, &manifest)
}

async fn serve_tarball(
    state: &SharedState,
    headers: &HeaderMap,
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
    // probes via `..` in the tarball segment and gives us the size+sha for the
    // streaming response headers.
    let package_owned = package.to_string();
    let version_owned = version.clone();
    let meta = db_blocking(state, move |db| {
        regdb::lookup_tarball(db, &package_owned, &version_owned)
    })
    .await?;
    let Some(mut meta) = meta else {
        return Err(AppError::NotFound(format!("{package}-{version}")));
    };

    // Proxy passthrough: a tarball_size of 0 means upsert_proxy_packument
    // recorded the version (sub-PR 2) but never had the actual bytes. Fetch
    // from upstream now, store, and update the row so subsequent reads hit
    // the hot tier. Failures here pass through as 404 — the client gets a
    // chance to retry; the operator gets a tracing warn.
    if meta.size == 0 {
        let package_owned = package.to_string();
        let version_owned = version.clone();
        let proxy_state = db_blocking(state, move |db| {
            regdb::lookup_proxy_tarball_state(db, &package_owned, &version_owned)
        })
        .await?;
        if let Some(ps) = proxy_state {
            if ps.is_proxy {
                let cfg =
                    db_blocking(state, move |db| Ok(upstream::load_config(db))).await?;
                if cfg.enabled {
                    if let Some(url) = ps.upstream_tarball_url.as_deref() {
                        tracing::debug!(%package, %version, %url, "proxy tarball miss → upstream fetch");
                        match upstream::fetch_tarball(url).await {
                            Ok(Some(resp)) => {
                                let bytes = resp
                                    .bytes()
                                    .await
                                    .map_err(|e| {
                                        tracing::warn!(%package, "upstream tarball read failed: {e:#}");
                                        AppError::NotFound(format!("{package}/{tarball}"))
                                    })?;
                                let body = bytes.to_vec();
                                let new_sha =
                                    storage::write_tarball(state, package, tarball, body)
                                        .await?;
                                let new_size = bytes.len() as i64;
                                let package_owned = package.to_string();
                                let version_owned = version.clone();
                                let sha_owned = new_sha.clone();
                                db_blocking(state, move |db| {
                                    regdb::finalize_proxy_tarball(
                                        db,
                                        &package_owned,
                                        &version_owned,
                                        new_size,
                                        &sha_owned,
                                    )
                                })
                                .await?;
                                meta.size = new_size;
                                meta.sha512 = new_sha;
                            }
                            Ok(None) => {
                                tracing::info!(%package, %version, "upstream 404 for tarball");
                                return Err(AppError::NotFound(format!("{package}/{tarball}")));
                            }
                            Err(e) => {
                                tracing::warn!(%package, "upstream tarball fetch failed: {e:#}");
                                return Err(AppError::NotFound(format!("{package}/{tarball}")));
                            }
                        }
                    }
                }
            }
        }
    }

    let etag = format!("\"{}\"", meta.sha512);
    // Conditional GET: the tarball is immutable per (package, version) so a
    // matching If-None-Match means the client already has the right bytes.
    if if_none_match_matches(headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(ETAG, etag)]).into_response());
    }

    let (file, size) = storage::open_tarball_stream(state, package, tarball)
        .await
        .map_err(|e| {
            tracing::error!("registry: open_tarball_stream failed: {e:#}");
            AppError::NotFound(format!("{package}/{tarball}"))
        })?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/octet-stream".to_string()),
            (CONTENT_LENGTH, size.to_string()),
            (ETAG, etag),
        ],
        body,
    )
        .into_response())
}

/// HEAD-tarball: identical headers to GET, but no body. pnpm and bun probe
/// with HEAD before downloading large tarballs.
async fn serve_tarball_head(
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
    let package_owned = package.to_string();
    let version_owned = version.clone();
    let meta = db_blocking(state, move |db| {
        regdb::lookup_tarball(db, &package_owned, &version_owned)
    })
    .await?;
    let Some(meta) = meta else {
        return Err(AppError::NotFound(format!("{package}-{version}")));
    };
    let etag = format!("\"{}\"", meta.sha512);
    Ok((
        StatusCode::OK,
        [
            (CONTENT_TYPE, "application/octet-stream".to_string()),
            (CONTENT_LENGTH, meta.size.to_string()),
            (ETAG, etag),
        ],
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

    // Sanity-check the filename. Two forms are accepted because npm CLIs are
    // inconsistent for scoped packages:
    // - the canonical (and on-disk) form is the unscoped basename:
    //     `fixture-1.0.0.tgz`
    // - npm CLI publish uses the full scoped key:
    //     `@pier-smoke/fixture-1.0.0.tgz`
    // We accept either, then continue using the short form everywhere downstream
    // (FS layout, `dist.tarball` URL).
    let expected_short = registry::tarball_filename(package, &version);
    let expected_full = format!("{package}-{version}.tgz");
    if !attachment_name_matches(attachment_name, &expected_short, &expected_full) {
        return Err(AppError::BadRequest(format!(
            "attachment name '{attachment_name}' does not match expected '{expected_short}' or '{expected_full}'"
        )));
    }
    let expected_name = expected_short;

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

    // DB transaction (merge dist-tags + insert version) on the blocking pool.
    let package_for_db = package.to_string();
    let version_for_db = version.clone();
    let description_for_db = description.clone();
    let manifest_for_db = manifest_json.clone();
    let integrity_for_db = computed_integrity.clone();
    let user_id_for_db = user.id.clone();
    let state_cl = state.clone();
    let insert_result = tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let db = state_cl
            .db
            .lock()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("DB lock poisoned: {e}")))?;
        let mut merged: BTreeMap<String, String> = db
            .query_row(
                "SELECT dist_tags_json FROM npm_packages WHERE name = ?1",
                [&package_for_db],
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
            &package_for_db,
            &description_for_db,
            &version_for_db,
            &manifest_for_db,
            tarball_size,
            &integrity_for_db,
            Some(&user_id_for_db),
            &merged,
        ) {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("UNIQUE") {
                    Err(AppError::Conflict(format!(
                        "{package_for_db}@{version_for_db} already published"
                    )))
                } else {
                    Err(AppError::Internal(e))
                }
            }
        }
    })
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task: {e}")))?;
    insert_result?;

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

// ----- dist-tag / unpublish / deprecate implementations ---------------------

async fn serve_dist_tags(
    state: &SharedState,
    package: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let package_owned = package.to_string();
    let tags = db_blocking(state, move |db| regdb::load_dist_tags(db, &package_owned)).await?;
    let Some(tags) = tags else {
        return Err(AppError::NotFound(format!("package {package}")));
    };
    let value = serde_json::to_value(tags).map_err(anyhow::Error::from)?;
    Ok((
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&value).map_err(anyhow::Error::from)?,
    )
        .into_response())
}

async fn handle_set_dist_tag(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
    tag: &str,
    body: &[u8],
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    let version = parse_version_body(body)?;
    require_can_modify(state, user, package, Some(&version)).await?;
    let package_owned = package.to_string();
    let tag_owned = tag.to_string();
    let version_owned = version.clone();
    let result = db_blocking(state, move |db| {
        regdb::set_dist_tag(db, &package_owned, &tag_owned, &version_owned)
    })
    .await;
    map_db_user_error(result)?;
    tracing::info!(
        "registry: set dist-tag {package}@{tag} -> {version} by {}",
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response())
}

async fn handle_remove_dist_tag(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
    tag: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    require_can_modify(state, user, package, None).await?;
    let package_owned = package.to_string();
    let tag_owned = tag.to_string();
    let result = db_blocking(state, move |db| {
        regdb::remove_dist_tag(db, &package_owned, &tag_owned)
    })
    .await;
    let removed = map_db_user_error(result)?;
    if !removed {
        return Err(AppError::NotFound(format!("dist-tag {tag}")));
    }
    tracing::info!(
        "registry: removed dist-tag {package}@{tag} by {}",
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response())
}

async fn handle_delete_package(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    require_can_modify(state, user, package, None).await?;
    let package_owned = package.to_string();
    let removed = db_blocking(state, move |db| regdb::delete_package(db, &package_owned)).await?;
    if removed.is_empty() {
        return Err(AppError::NotFound(format!("package {package}")));
    }
    // Drop the hot-tier blobs. Cold-tier (S3) blobs are deliberately left in
    // place — operators can sweep them with an S3 lifecycle policy.
    for r in &removed {
        let _ = storage::delete_tarball(state, &r.package, &r.filename).await;
    }
    tracing::info!(
        "registry: unpublished {package} ({} versions) by {}",
        removed.len(),
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response())
}

async fn handle_delete_version(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
    tarball: &str,
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    if !tarball.ends_with(".tgz") {
        return Err(AppError::BadRequest("tarball must end in .tgz".into()));
    }
    let version = derive_version_from_tarball(package, tarball)
        .ok_or_else(|| AppError::BadRequest("malformed tarball name".into()))?;
    require_can_modify(state, user, package, Some(&version)).await?;
    let package_owned = package.to_string();
    let version_owned = version.clone();
    let removed = db_blocking(state, move |db| {
        regdb::delete_version(db, &package_owned, &version_owned)
    })
    .await?;
    let Some(removed) = removed else {
        return Err(AppError::NotFound(format!("{package}@{version}")));
    };
    let _ = storage::delete_tarball(state, &removed.package, &removed.filename).await;
    tracing::info!(
        "registry: unpublished {package}@{version} by {}",
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response())
}

async fn handle_deprecate(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
    body: &[u8],
) -> Result<axum::response::Response, AppError> {
    validate_package_name(package)?;
    require_can_modify(state, user, package, None).await?;

    let body_json: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| AppError::BadRequest(format!("invalid json: {e}")))?;

    // `npm deprecate` PUTs the entire packument with `versions[*].deprecated`
    // patched. We diff against the DB and apply only the deprecation flips —
    // everything else in the body is ignored, so a stale rev can't accidentally
    // rewrite a manifest.
    let versions = body_json
        .get("versions")
        .and_then(|v| v.as_object())
        .ok_or_else(|| AppError::BadRequest("missing versions{}".into()))?;

    let mut messages: BTreeMap<String, String> = BTreeMap::new();
    for (ver, manifest) in versions {
        let dep = manifest.get("deprecated").and_then(|v| v.as_str());
        if let Some(msg) = dep {
            messages.insert(ver.clone(), msg.to_string());
        } else {
            // Field absent → clear the deprecation flag.
            messages.insert(ver.clone(), String::new());
        }
    }
    if messages.is_empty() {
        return Err(AppError::BadRequest("nothing to deprecate".into()));
    }

    let package_owned = package.to_string();
    let messages_for_db = messages.clone();
    db_blocking(state, move |db| {
        regdb::deprecate_versions(db, &package_owned, &messages_for_db)
    })
    .await?;

    let n_marked = messages.values().filter(|v| !v.is_empty()).count();
    let n_cleared = messages.len() - n_marked;
    tracing::info!(
        "registry: deprecate {package} (+{n_marked} -{n_cleared}) by {}",
        user.username
    );
    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response())
}

/// Parse the body of `PUT /-/package/.../dist-tags/{tag}` — npm sends the
/// version as a JSON-encoded string (`"1.0.0"`). Tolerant of plain `1.0.0`
/// too because pnpm and yarn sometimes drop the quotes.
fn parse_version_body(body: &[u8]) -> Result<String, AppError> {
    let raw = std::str::from_utf8(body)
        .map_err(|_| AppError::BadRequest("body not utf-8".into()))?
        .trim();
    let version = if let Ok(parsed) = serde_json::from_str::<String>(raw) {
        parsed
    } else {
        raw.trim_matches('"').to_string()
    };
    if version.is_empty() {
        return Err(AppError::BadRequest("missing version in body".into()));
    }
    Ok(version)
}

/// Convert anyhow-flavoured user errors thrown by `registry::db` into the
/// right HTTP status — they're business-rule violations, not 500s. Anything
/// we don't recognise stays Internal.
fn map_db_user_error<T>(result: Result<T, AppError>) -> Result<T, AppError> {
    let Err(AppError::Internal(e)) = result else {
        return result;
    };
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("invalid dist-tag")
        || lower.contains("does not exist")
        || lower.contains("refusing to remove")
        || lower.contains("package not found")
    {
        Err(AppError::BadRequest(msg))
    } else {
        Err(AppError::Internal(e))
    }
}

/// Authorise a mutation. Admin role always passes. Otherwise:
/// - If `version` is given, the user must be the publisher of that version
///   (or no `published_by` is recorded — legacy rows pre-dating the column).
/// - If no `version`, the user must have published at least one version of
///   the package (or no version has a recorded publisher at all).
async fn require_can_modify(
    state: &SharedState,
    user: &AuthUser,
    package: &str,
    version: Option<&str>,
) -> Result<(), AppError> {
    if user
        .global_role
        .at_least(crate::auth::rbac::GlobalRole::Admin)
    {
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

// ----- helpers ----------------------------------------------------------------

/// Reject obviously bad package names early (path traversal, empty, etc).
/// The full npm validity rules live in `validate-npm-package-name` —
/// we only enforce the security-critical subset here.
pub fn validate_package_name(name: &str) -> Result<(), AppError> {
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

/// Serialise a JSON body, compute its ETag, and either return 304 (if the
/// client's `If-None-Match` matches) or 200 with the body + headers.
fn json_response(
    headers: &HeaderMap,
    value: &serde_json::Value,
) -> Result<axum::response::Response, AppError> {
    let bytes = serde_json::to_vec(value).map_err(anyhow::Error::from)?;
    let etag = compute_etag(&bytes);
    if if_none_match_matches(headers, &etag) {
        return Ok((StatusCode::NOT_MODIFIED, [(ETAG, etag)]).into_response());
    }
    Ok((
        StatusCode::OK,
        [(CONTENT_TYPE, "application/json".to_string()), (ETAG, etag)],
        bytes,
    )
        .into_response())
}

/// Check whether the `_attachments` key from a publish body matches either of
/// the two filename forms a real npm client may send:
/// - `expected_short`: the bare basename used by Pier on disk and in
///   `dist.tarball` (`fixture-1.0.0.tgz`).
/// - `expected_full`: the scoped form npm CLI actually transmits
///   (`@scope/fixture-1.0.0.tgz`).
///
/// Pulled out of `handle_publish` so the matching rule is unit-testable.
fn attachment_name_matches(actual: &str, expected_short: &str, expected_full: &str) -> bool {
    actual == expected_short || actual == expected_full
}

/// True when the client opts into the abbreviated packument via Accept.
fn wants_abbreviated(headers: &HeaderMap) -> bool {
    headers
        .get(ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains(ABBREVIATED_MEDIA_TYPE))
        .unwrap_or(false)
}

/// Compute a weakish ETag from the response body. Strong sha256 truncated to
/// 16 bytes (128 bits) — collision-resistant for any realistic registry, and
/// short enough to keep header overhead tiny.
fn compute_etag(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

/// Check whether an `If-None-Match` header matches our ETag. Supports the
/// comma-separated list form and the `*` wildcard.
fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers.get(IF_NONE_MATCH).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    value.split(',').any(|tag| {
        let t = tag.trim();
        t == etag || t == "*"
    })
}

/// Build the abbreviated packument response. Keeps only install-time fields,
/// dropping README, author, contributors and other docs that bloat the payload.
fn build_abbreviated_packument(packument: &regdb::Packument) -> serde_json::Value {
    let mut versions = serde_json::Map::with_capacity(packument.versions.len());
    for (ver, manifest) in &packument.versions {
        let mut abridged = serde_json::Map::new();
        if let Some(obj) = manifest.as_object() {
            for key in ABBREVIATED_KEYS {
                if let Some(v) = obj.get(*key) {
                    abridged.insert((*key).to_string(), v.clone());
                }
            }
        }
        versions.insert(ver.clone(), serde_json::Value::Object(abridged));
    }
    let modified = packument.time.values().max().cloned().unwrap_or_default();
    serde_json::json!({
        "name": packument.name,
        "modified": modified,
        "dist-tags": packument.dist_tags,
        "versions": versions,
    })
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

    #[test]
    fn etag_is_stable_and_unique() {
        let a = compute_etag(b"hello");
        let b = compute_etag(b"hello");
        let c = compute_etag(b"world");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with('"') && a.ends_with('"'));
        // Truncated sha256 → 32 hex chars + 2 quote chars.
        assert_eq!(a.len(), 34);
    }

    #[test]
    fn if_none_match_handles_lists_and_wildcard() {
        let etag = compute_etag(b"x");
        let mut h = HeaderMap::new();
        h.insert(IF_NONE_MATCH, etag.parse().unwrap());
        assert!(if_none_match_matches(&h, &etag));

        let other = compute_etag(b"y");
        h.insert(IF_NONE_MATCH, other.parse().unwrap());
        assert!(!if_none_match_matches(&h, &etag));

        let list = format!("{other}, {etag}");
        h.insert(IF_NONE_MATCH, list.parse().unwrap());
        assert!(if_none_match_matches(&h, &etag));

        h.insert(IF_NONE_MATCH, "*".parse().unwrap());
        assert!(if_none_match_matches(&h, &etag));
    }

    #[test]
    fn wants_abbreviated_detects_media_type() {
        let mut h = HeaderMap::new();
        assert!(!wants_abbreviated(&h));
        h.insert(ACCEPT, "application/json".parse().unwrap());
        assert!(!wants_abbreviated(&h));
        h.insert(
            ACCEPT,
            "application/vnd.npm.install-v1+json".parse().unwrap(),
        );
        assert!(wants_abbreviated(&h));
        h.insert(
            ACCEPT,
            "application/vnd.npm.install-v1+json, */*".parse().unwrap(),
        );
        assert!(wants_abbreviated(&h));
    }

    #[test]
    fn attachment_name_accepts_both_forms() {
        let short = "fixture-1.0.0.tgz";
        let full = "@pier-smoke/fixture-1.0.0.tgz";
        // Unscoped clients (and the spec-canonical key) send the short form.
        assert!(attachment_name_matches(short, short, full));
        // Real npm CLI sends the full scoped form even though `dist.tarball`
        // and the on-disk filename use the short form — accept it.
        assert!(attachment_name_matches(full, short, full));
        // Anything else is rejected.
        assert!(!attachment_name_matches("other-1.0.0.tgz", short, full));
        assert!(!attachment_name_matches("../../etc/passwd", short, full));
        assert!(!attachment_name_matches("", short, full));
    }

    #[test]
    fn parse_version_body_accepts_quoted_and_raw() {
        // npm sends a JSON-encoded string for `npm dist-tag add ... <tag>`.
        assert_eq!(parse_version_body(b"\"1.2.3\"").unwrap(), "1.2.3");
        // pnpm / yarn classic sometimes drop the quotes.
        assert_eq!(parse_version_body(b"1.2.3").unwrap(), "1.2.3");
        // Whitespace tolerated either way.
        assert_eq!(parse_version_body(b"  \"4.5.6\"  ").unwrap(), "4.5.6");
        // Empty / blank body is a 400.
        assert!(parse_version_body(b"").is_err());
        assert!(parse_version_body(b"\"\"").is_err());
        // Non-utf-8 body is a 400, not a panic.
        assert!(parse_version_body(&[0xff, 0xfe, 0xfd]).is_err());
    }

    #[test]
    fn abbreviated_strips_non_install_fields() {
        let mut versions = BTreeMap::new();
        versions.insert(
            "1.0.0".to_string(),
            serde_json::json!({
                "name": "foo",
                "version": "1.0.0",
                "readme": "a very long readme here...",
                "author": { "name": "Alice" },
                "contributors": ["Bob"],
                "keywords": ["x", "y"],
                "dependencies": { "bar": "^2.0.0" },
                "dist": {
                    "tarball": "https://pier/registry/npm/foo/-/foo-1.0.0.tgz",
                    "integrity": "sha512-abc",
                    "shasum": "deadbeef"
                }
            }),
        );
        let mut time = BTreeMap::new();
        time.insert("1.0.0".to_string(), "2026-05-12T00:00:00Z".to_string());

        let pkg = regdb::Packument {
            name: "foo".to_string(),
            description: "x".to_string(),
            dist_tags: {
                let mut m = BTreeMap::new();
                m.insert("latest".to_string(), "1.0.0".to_string());
                m
            },
            is_proxy: false,
            versions,
            time,
        };

        let abr = build_abbreviated_packument(&pkg);
        let v1 = abr
            .get("versions")
            .and_then(|v| v.get("1.0.0"))
            .and_then(|v| v.as_object())
            .expect("version 1.0.0 present");
        assert!(v1.contains_key("dependencies"));
        assert!(v1.contains_key("dist"));
        assert!(!v1.contains_key("readme"));
        assert!(!v1.contains_key("author"));
        assert!(!v1.contains_key("contributors"));
        assert!(!v1.contains_key("keywords"));
        assert_eq!(
            abr.get("modified").and_then(|v| v.as_str()),
            Some("2026-05-12T00:00:00Z")
        );
        assert_eq!(
            abr.get("dist-tags")
                .and_then(|v| v.get("latest"))
                .and_then(|v| v.as_str()),
            Some("1.0.0")
        );
    }
}
