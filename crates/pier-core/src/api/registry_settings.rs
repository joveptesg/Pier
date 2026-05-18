//! Settings for the embedded npm registry.
//!
//! Exposes:
//! - `s3_storage_id` — which row of `s3_storages` to mirror tarballs into.
//!   Empty/null = no cold-tier mirroring.
//! - `proxy.*` — upstream proxy/mirror knobs (see registry/upstream.rs).
//!   `enabled`, `upstream_url`, `ttl_seconds`, `max_cache_size_mb`.
//!
//! Stored in the generic `settings` key/value table so we don't churn
//! migrations every time a new toggle is added.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use rusqlite::params;
use serde::Deserialize;

use crate::error::AppResult;
use crate::registry::upstream;
use crate::state::SharedState;

const KEY_S3_STORAGE_ID: &str = "registry.s3_storage_id";

#[derive(Deserialize)]
pub struct UpdateRequest {
    pub s3_storage_id: Option<String>,
    /// Upstream proxy/mirror settings. All fields are optional — only the
    /// ones present in the request are touched. `proxy_enabled = false`
    /// keeps the cached rows; flipping back on resumes from where we left.
    pub proxy_enabled: Option<bool>,
    pub proxy_upstream_url: Option<String>,
    pub proxy_ttl_seconds: Option<u64>,
    pub proxy_max_cache_size_mb: Option<u64>,
}

/// `GET /api/v1/registry/settings`.
pub async fn get(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let s3_id: Option<String> = db
        .query_row(
            "SELECT value FROM settings WHERE key = ?1",
            [KEY_S3_STORAGE_ID],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|s| !s.is_empty());

    let proxy_cfg = upstream::load_config(&db);
    let (cached_packages, cached_size_bytes) = db
        .query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(v.tarball_size), 0)
               FROM npm_packages p
               LEFT JOIN npm_versions v ON v.package_name = p.name
              WHERE p.is_proxy = 1",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
        )
        .unwrap_or((0, 0));

    Ok(Json(serde_json::json!({
        "s3_storage_id": s3_id,
        "proxy": {
            "enabled": proxy_cfg.enabled,
            "upstream_url": proxy_cfg.upstream_url,
            "ttl_seconds": proxy_cfg.ttl_seconds,
            "max_cache_size_mb": proxy_cfg.max_cache_size_mb,
            "cached_packages": cached_packages,
            "cached_size_bytes": cached_size_bytes,
        }
    })))
}

/// `PUT /api/v1/registry/settings`. Empty/null `s3_storage_id` clears it,
/// disabling cold-tier mirroring. Validates the id actually points at an
/// existing s3_storages row before saving.
pub async fn update(
    State(state): State<SharedState>,
    Json(body): Json<UpdateRequest>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let normalized = body
        .s3_storage_id
        .as_deref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    if let Some(id) = normalized {
        let exists: bool = db
            .query_row("SELECT 1 FROM s3_storages WHERE id = ?1", [id], |_| {
                Ok(true)
            })
            .unwrap_or(false);
        if !exists {
            return Err(crate::error::AppError::BadRequest(format!(
                "s3 storage '{id}' not found"
            )));
        }
    }

    let value = normalized.unwrap_or("");
    db.execute(
        "INSERT INTO settings (key, value, updated_at)
         VALUES (?1, ?2, datetime('now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = datetime('now')",
        params![KEY_S3_STORAGE_ID, value],
    )?;

    // Proxy fields. Each is optional so partial PUTs work — useful for the
    // UI which toggles `enabled` independently from the URL/TTL inputs.
    if let Some(enabled) = body.proxy_enabled {
        upstream::put_setting(
            &db,
            upstream::SETTING_ENABLED,
            if enabled { "true" } else { "false" },
        )?;
    }
    if let Some(url) = body.proxy_upstream_url.as_deref() {
        let trimmed = url.trim().trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(crate::error::AppError::BadRequest(
                "proxy_upstream_url cannot be empty".into(),
            ));
        }
        if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
            return Err(crate::error::AppError::BadRequest(
                "proxy_upstream_url must start with http:// or https://".into(),
            ));
        }
        upstream::put_setting(&db, upstream::SETTING_UPSTREAM_URL, trimmed)?;
    }
    if let Some(ttl) = body.proxy_ttl_seconds {
        upstream::put_setting(&db, upstream::SETTING_TTL_SECONDS, &ttl.to_string())?;
    }
    if let Some(max_mb) = body.proxy_max_cache_size_mb {
        upstream::put_setting(
            &db,
            upstream::SETTING_MAX_CACHE_SIZE_MB,
            &max_mb.to_string(),
        )?;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

/// `GET /api/v1/registry/proxy/packages` — list every package the cache
/// pulled from upstream (`is_proxy = 1`). Used by the Upstream proxy UI tab
/// so operators can see what's actually in the mirror without dropping to
/// SQL. The list reuses `PackageSummary` (size/version count already
/// aggregated) and adds nothing private — proxy entries only.
pub async fn proxy_packages_list(
    State(state): State<SharedState>,
) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let all = crate::registry::db::list_packages(&db, false)?;
    let proxy_only: Vec<_> = all.into_iter().filter(|p| p.is_proxy).collect();
    Ok(Json(proxy_only))
}
