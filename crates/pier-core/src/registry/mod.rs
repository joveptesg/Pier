//! Embedded npm-compatible registry.
//!
//! Storage is hybrid: tarballs live on local FS as a hot tier and are
//! mirrored asynchronously to S3 (cold tier) when an S3 storage is
//! configured. Metadata is in SQLite (`npm_packages`, `npm_versions`).
//!
//! See [`crate::api::npm`] for the HTTP handlers and
//! `C:\Users\user\.claude\plans\verdaccio-stateless-aho.md` for design notes.

pub mod db;
pub mod storage;
pub mod upstream;

use std::path::PathBuf;

use crate::config::PierConfig;

/// On-disk root for the hot-tier tarball cache.
/// Layout: `{data_dir}/registry/{package}/{tarball-filename}.tgz`.
pub fn fs_root(config: &PierConfig) -> PathBuf {
    config.data_dir.join("registry")
}

/// S3 key prefix for a tarball, paired with the configured `key_prefix` of
/// the chosen storage row. Lives alongside backups under `<bucket>/<prefix>/registry/...`.
pub fn s3_key(storage_prefix: &str, package: &str, tarball: &str) -> String {
    let prefix = storage_prefix.trim_matches('/');
    if prefix.is_empty() {
        format!("registry/{package}/{tarball}")
    } else {
        format!("{prefix}/registry/{package}/{tarball}")
    }
}

/// Tarball filename as advertised in `dist.tarball` URLs.
/// Scoped: `@scope/name@1.2.3` → `name-1.2.3.tgz` (scope is in the URL path,
/// not the filename — matching what npm CLI expects).
pub fn tarball_filename(package: &str, version: &str) -> String {
    let unscoped = package.rsplit_once('/').map(|(_, n)| n).unwrap_or(package);
    format!("{unscoped}-{version}.tgz")
}
