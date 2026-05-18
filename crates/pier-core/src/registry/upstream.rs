//! Upstream proxy/cache for the embedded npm registry.
//!
//! When `registry.proxy.enabled = true`, missing packages are fetched from a
//! configured upstream (default `registry.npmjs.org`), URL-rewritten so
//! `dist.tarball` points at us, and cached in `npm_packages` /
//! `npm_versions` with `is_proxy = 1`.
//!
//! This module ships the building blocks (config loader, upstream HTTP
//! client, URL-rewrite helper) — route wiring lands in a follow-up PR so
//! reviewers can audit the network surface in isolation.
//!
//! The cache columns (`is_proxy`, `upstream_etag`, `upstream_fetched_at`)
//! already exist on `npm_packages` from the initial schema; no migration is
//! needed for this sub-PR.

// Foundation layer: every public item below is consumed by sub-PR 2 (route
// wiring). Suppressed here so this PR lands clean against `-D warnings`
// without forward references.
#![allow(dead_code)]

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Arc;
use std::time::Duration;

use crate::registry::{db as regdb, storage};
use crate::state::AppState;

/// Settings key prefix — colocates all proxy knobs under one namespace so the
/// future "/packages → Proxy" tab can list them with `WHERE key LIKE …`.
pub const SETTING_ENABLED: &str = "registry.proxy.enabled";
pub const SETTING_UPSTREAM_URL: &str = "registry.proxy.upstream_url";
pub const SETTING_TTL_SECONDS: &str = "registry.proxy.ttl_seconds";
pub const SETTING_MAX_CACHE_SIZE_MB: &str = "registry.proxy.max_cache_size_mb";

/// Fallback upstream — the public npm registry. Operators on air-gapped or
/// audit-constrained networks point this at an internal mirror.
pub const DEFAULT_UPSTREAM: &str = "https://registry.npmjs.org";

/// Default packument refresh window. 10 minutes is long enough to absorb
/// the install-spike that follows a tag promotion (`npm dist-tag add latest …`)
/// without serving badly-stale data to long-lived CI hosts.
pub const DEFAULT_TTL_SECONDS: u64 = 600;

/// Resolved proxy configuration. Loaded once per request boundary so the
/// hot read path doesn't go back to SQLite for every `dist.tarball` URL.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    pub enabled: bool,
    pub upstream_url: String,
    pub ttl_seconds: u64,
    pub max_cache_size_mb: u64,
}

impl ProxyConfig {
    /// Disabled-by-default fallback used when settings rows are missing.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            upstream_url: DEFAULT_UPSTREAM.to_string(),
            ttl_seconds: DEFAULT_TTL_SECONDS,
            max_cache_size_mb: 0,
        }
    }
}

/// Read the proxy configuration from the `settings` K/V table.
///
/// Missing or malformed rows fall back to safe defaults rather than aborting
/// the request — a proxy mis-configuration must never break private publish.
pub fn load_config(db: &Connection) -> ProxyConfig {
    let enabled = read_bool(db, SETTING_ENABLED).unwrap_or(false);
    let upstream_url =
        read_string(db, SETTING_UPSTREAM_URL).unwrap_or_else(|| DEFAULT_UPSTREAM.to_string());
    let ttl_seconds = read_u64(db, SETTING_TTL_SECONDS).unwrap_or(DEFAULT_TTL_SECONDS);
    let max_cache_size_mb = read_u64(db, SETTING_MAX_CACHE_SIZE_MB).unwrap_or(0);
    ProxyConfig {
        enabled,
        upstream_url: upstream_url.trim_end_matches('/').to_string(),
        ttl_seconds,
        max_cache_size_mb,
    }
}

/// Persist a single proxy setting. The UI handler in sub-PR 4 wires this up.
pub fn put_setting(db: &Connection, key: &str, value: &str) -> Result<()> {
    db.execute(
        "INSERT OR REPLACE INTO settings (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .with_context(|| format!("writing setting {key}"))?;
    Ok(())
}

fn read_string(db: &Connection, key: &str) -> Option<String> {
    db.query_row(
        "SELECT value FROM settings WHERE key = ?1",
        params![key],
        |r| r.get::<_, String>(0),
    )
    .optional()
    .ok()
    .flatten()
}

fn read_bool(db: &Connection, key: &str) -> Option<bool> {
    read_string(db, key).map(|v| matches!(v.as_str(), "true" | "1" | "yes" | "on"))
}

fn read_u64(db: &Connection, key: &str) -> Option<u64> {
    read_string(db, key)?.parse().ok()
}

/// Raw upstream packument fetch result: body + ETag (if upstream sent one).
/// Caller decides what to do with the JSON — typical path is rewrite-and-cache.
#[derive(Debug, Clone)]
pub struct UpstreamPackument {
    pub body: serde_json::Value,
    pub etag: Option<String>,
}

/// Fetch a packument from upstream. Returns `None` if upstream replied 404
/// (so the caller can pass that through cleanly instead of dressing it up
/// as a 5xx). Other non-2xx responses surface as `Err`.
///
/// `If-None-Match` is the caller's responsibility — we don't carry it here
/// because the proxy-cache TTL gate happens at a higher layer and a stale
/// cached row can short-circuit the upstream call entirely.
pub async fn fetch_packument(
    upstream_url: &str,
    name: &str,
    accept_abbreviated: bool,
) -> Result<Option<UpstreamPackument>> {
    let client = client()?;
    let url = format!("{}/{}", upstream_url, urlencode_pkg(name));
    let accept = if accept_abbreviated {
        "application/vnd.npm.install-v1+json"
    } else {
        "application/json"
    };
    let resp = client
        .get(&url)
        .header(reqwest::header::ACCEPT, accept)
        .header(reqwest::header::USER_AGENT, user_agent())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("upstream {url} returned {}", resp.status());
    }
    let etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = resp
        .json::<serde_json::Value>()
        .await
        .with_context(|| format!("parsing packument from {url}"))?;
    Ok(Some(UpstreamPackument { body, etag }))
}

/// Open a streaming download for an upstream tarball. Caller pipes the
/// response into the local FS + S3 layer so memory stays bounded regardless
/// of tarball size. Returns `None` on 404 (passthrough), `Err` on transport
/// failure / other non-2xx status.
pub async fn fetch_tarball(url: &str) -> Result<Option<reqwest::Response>> {
    let client = client()?;
    let resp = client
        .get(url)
        .header(reqwest::header::USER_AGENT, user_agent())
        .send()
        .await
        .with_context(|| format!("GET tarball {url}"))?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("upstream tarball {url} returned {}", resp.status());
    }
    Ok(Some(resp))
}

/// Rewrite every `versions[*].dist.tarball` so they point at our own
/// `/registry/npm/{pkg}/-/{tarball}` route instead of the upstream origin.
/// This is what makes proxy mode transparent to npm/yarn/pnpm/bun — they
/// always hit Pier for the actual bytes, and we cache or stream as needed.
///
/// Idempotent: re-running on an already-rewritten packument is a no-op.
pub fn rewrite_tarball_urls(packument: &mut serde_json::Value, public_base: &str, pkg: &str) {
    let Some(versions) = packument
        .get_mut("versions")
        .and_then(|v| v.as_object_mut())
    else {
        return;
    };
    let base = public_base.trim_end_matches('/');
    for (_ver, manifest) in versions.iter_mut() {
        let Some(dist) = manifest.get_mut("dist").and_then(|d| d.as_object_mut()) else {
            continue;
        };
        // Use the upstream filename basename so the on-disk layout matches
        // the canonical `<pkg>-<ver>.tgz` shape Pier already uses.
        let Some(upstream_url) = dist.get("tarball").and_then(|v| v.as_str()) else {
            continue;
        };
        let filename = upstream_url
            .rsplit_once('/')
            .map(|(_, f)| f.to_string())
            .unwrap_or_else(|| upstream_url.to_string());
        let rewritten = format!(
            "{}/registry/npm/{}/-/{}",
            base,
            urlencode_pkg(pkg),
            filename
        );
        dist.insert("tarball".into(), serde_json::Value::String(rewritten));
    }
}

fn client() -> Result<reqwest::Client> {
    // Modest timeout — npmjs.org is fast under normal conditions; a stuck
    // upstream should fail fast so the caller can fall back to a stale
    // cached row rather than blocking the user-facing install.
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(user_agent())
        .build()
        .context("building upstream http client")
}

fn user_agent() -> String {
    format!("pier-registry-proxy/{}", env!("CARGO_PKG_VERSION"))
}

/// npm registry expects scoped names URL-encoded (`@scope%2Fname`). The
/// flat case round-trips through `urlencoding::encode` unchanged.
fn urlencode_pkg(name: &str) -> String {
    if let Some(rest) = name.strip_prefix('@') {
        if let Some((scope, pkg)) = rest.split_once('/') {
            return format!("@{scope}%2F{pkg}");
        }
    }
    name.to_string()
}

/// Stats returned by `run_gc` — useful for tracing and a future
/// "/api/v1/registry/proxy/stats" endpoint.
#[derive(Debug, Clone, Default)]
pub struct EvictionStats {
    pub evicted_count: usize,
    pub freed_bytes: i64,
}

/// Enforce `registry.proxy.max_cache_size_mb`. Picks the oldest cached
/// tarballs (FIFO on published_at) and deletes them from the hot tier until
/// the total fits inside the cap, then resets each version row's
/// `tarball_size = 0` so the next request transparently re-fetches.
///
/// No-op when proxy is disabled or `max_cache_size_mb = 0` (unlimited).
/// Errors on individual evictions are logged but never abort the sweep —
/// a partially-completed GC is better than a stalled one.
pub async fn run_gc(state: &Arc<AppState>) -> Result<EvictionStats> {
    let cfg = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock for GC: {e}"))?;
        load_config(&db)
    };
    if !cfg.enabled || cfg.max_cache_size_mb == 0 {
        return Ok(EvictionStats::default());
    }
    let max_bytes = (cfg.max_cache_size_mb as i64).saturating_mul(1024 * 1024);

    // Pick eviction targets via spawn_blocking so the tokio runtime isn't
    // stalled by the JOIN+scan over npm_versions.
    let state_for_pick = state.clone();
    let targets: Vec<(String, String, String)> = tokio::task::spawn_blocking(move || {
        let db = state_for_pick
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        regdb::pick_proxy_evictions(&db, max_bytes)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking: {e}"))??;

    let mut stats = EvictionStats::default();
    for (package, version, filename) in targets {
        if let Err(e) = storage::delete_local_tarball(state, &package, &filename).await {
            tracing::warn!(%package, %version, "proxy GC: tarball delete failed: {e:#}");
            // Fall through — still flip size=0 so the row stops being
            // counted toward the cap on the next sweep.
        }
        let state_for_db = state.clone();
        let pkg_owned = package.clone();
        let ver_owned = version.clone();
        let mark_res = tokio::task::spawn_blocking(move || {
            let db = state_for_db
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            regdb::mark_proxy_evicted(&db, &pkg_owned, &ver_owned)
        })
        .await;
        match mark_res {
            Ok(Ok(())) => {
                stats.evicted_count += 1;
            }
            Ok(Err(e)) => tracing::warn!(%package, %version, "proxy GC: mark_evicted failed: {e:#}"),
            Err(e) => tracing::warn!("proxy GC: spawn_blocking join: {e}"),
        }
    }
    if stats.evicted_count > 0 {
        tracing::info!(
            count = stats.evicted_count,
            "proxy GC: evicted {} cached tarball(s)",
            stats.evicted_count
        );
    }
    Ok(stats)
}

/// Spawn the periodic proxy-cache GC loop. Idle when proxy disabled or
/// `max_cache_size_mb = 0` — the loop still ticks but every iteration
/// no-ops cheaply. Called once at startup from `main.rs`.
pub fn spawn_gc_task(state: Arc<AppState>) {
    tokio::spawn(async move {
        // First tick after 5 minutes — never block startup on a sweep,
        // and let any boot-time misses cache up first.
        tokio::time::sleep(Duration::from_secs(300)).await;
        loop {
            if let Err(e) = run_gc(&state).await {
                tracing::warn!("proxy GC: sweep failed: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rewrites_dist_tarball_for_flat_packages() {
        let mut p = json!({
            "name": "react",
            "versions": {
                "18.0.0": {
                    "name": "react",
                    "version": "18.0.0",
                    "dist": {
                        "tarball": "https://registry.npmjs.org/react/-/react-18.0.0.tgz",
                        "integrity": "sha512-..."
                    }
                }
            }
        });
        rewrite_tarball_urls(&mut p, "https://pier.example", "react");
        assert_eq!(
            p["versions"]["18.0.0"]["dist"]["tarball"],
            "https://pier.example/registry/npm/react/-/react-18.0.0.tgz"
        );
        // integrity untouched
        assert_eq!(p["versions"]["18.0.0"]["dist"]["integrity"], "sha512-...");
    }

    #[test]
    fn rewrites_dist_tarball_for_scoped_packages() {
        let mut p = json!({
            "name": "@types/node",
            "versions": {
                "20.0.0": {
                    "dist": {
                        "tarball": "https://registry.npmjs.org/@types/node/-/node-20.0.0.tgz"
                    }
                }
            }
        });
        rewrite_tarball_urls(&mut p, "https://pier.example/", "@types/node");
        assert_eq!(
            p["versions"]["20.0.0"]["dist"]["tarball"],
            "https://pier.example/registry/npm/@types%2Fnode/-/node-20.0.0.tgz"
        );
    }

    #[test]
    fn rewrite_is_idempotent() {
        let mut p = json!({
            "versions": {
                "1.0.0": {
                    "dist": {"tarball": "https://registry.npmjs.org/x/-/x-1.0.0.tgz"}
                }
            }
        });
        rewrite_tarball_urls(&mut p, "https://pier.example", "x");
        let after_first = p["versions"]["1.0.0"]["dist"]["tarball"].clone();
        rewrite_tarball_urls(&mut p, "https://pier.example", "x");
        assert_eq!(p["versions"]["1.0.0"]["dist"]["tarball"], after_first);
    }

    #[test]
    fn rewrite_handles_missing_dist() {
        let mut p = json!({"versions": {"1.0.0": {"name": "x"}}});
        rewrite_tarball_urls(&mut p, "https://pier.example", "x");
        // no panic, no change
        assert!(p["versions"]["1.0.0"]["dist"].is_null());
    }

    #[test]
    fn proxy_config_disabled_by_default() {
        // Schema-bootstrapped DB with empty settings -> disabled, default URL.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        let cfg = load_config(&conn);
        assert!(!cfg.enabled);
        assert_eq!(cfg.upstream_url, DEFAULT_UPSTREAM);
        assert_eq!(cfg.ttl_seconds, DEFAULT_TTL_SECONDS);
    }

    #[test]
    fn proxy_config_reads_settings() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT)",
            [],
        )
        .unwrap();
        put_setting(&conn, SETTING_ENABLED, "true").unwrap();
        put_setting(&conn, SETTING_UPSTREAM_URL, "https://mirror.internal/").unwrap();
        put_setting(&conn, SETTING_TTL_SECONDS, "300").unwrap();
        put_setting(&conn, SETTING_MAX_CACHE_SIZE_MB, "1024").unwrap();
        let cfg = load_config(&conn);
        assert!(cfg.enabled);
        assert_eq!(cfg.upstream_url, "https://mirror.internal"); // trailing / stripped
        assert_eq!(cfg.ttl_seconds, 300);
        assert_eq!(cfg.max_cache_size_mb, 1024);
    }
}
