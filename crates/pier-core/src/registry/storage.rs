//! Hybrid hot/cold tarball storage.
//!
//! Hot tier: `{data_dir}/registry/{package}/{tarball}.tgz` on the local FS.
//! Cold tier: an S3-compatible bucket (using the first row in `s3_storages`,
//! if present), keyed under `{key_prefix}/registry/{package}/{tarball}`.
//!
//! Reads always go through the hot tier first. On a hot-tier miss we transparently
//! pull from S3 and rewrite the local file so subsequent reads are fast — this
//! is what lets a Pier instance survive a VPS reinstall without losing its
//! published packages.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha512};
use tokio::fs;

use crate::registry;
use crate::s3;
use crate::state::SharedState;

/// One configured S3 storage row, picked as the cold-tier target.
/// MVP: takes the first row (lowest `id`); operators with multiple buckets
/// can add an explicit "registry storage" pointer later.
#[derive(Clone)]
struct ColdTier {
    storage_type: String,
    endpoint: String,
    region: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    key_prefix: String,
}

fn package_dir(state: &SharedState, package: &str) -> PathBuf {
    // `@scope/name` becomes `@scope/name/` on disk — the slash is fine, both
    // POSIX and NTFS accept nested directories under a `@scope` parent.
    registry::fs_root(&state.config).join(package)
}

fn tarball_path(state: &SharedState, package: &str, filename: &str) -> PathBuf {
    package_dir(state, package).join(filename)
}

/// Best-effort delete of a hot-tier tarball. Used by the proxy LRU GC to
/// free disk while leaving the manifest row in place so the next request
/// re-fetches from upstream. A missing file (already gone) returns Ok —
/// the caller treats the eviction as complete either way.
pub async fn delete_local_tarball(
    state: &SharedState,
    package: &str,
    filename: &str,
) -> Result<()> {
    let path = tarball_path(state, package, filename);
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("remove {} failed: {e}", path.display())),
    }
}

/// Compute sha512 of a byte buffer in the npm-canonical "base64" form
/// (matches the `dist.integrity` field: `sha512-<base64>`).
pub fn integrity(bytes: &[u8]) -> String {
    use base64::Engine;
    let mut hasher = Sha512::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let b64 = base64::engine::general_purpose::STANDARD.encode(digest);
    format!("sha512-{b64}")
}

/// Persist a freshly published tarball to the hot tier and (best-effort) the
/// cold tier. Returns the integrity string. Atomic on the hot tier:
/// writes to a `.tmp` file then renames into place.
pub async fn write_tarball(
    state: &SharedState,
    package: &str,
    filename: &str,
    bytes: Vec<u8>,
) -> Result<String> {
    let dir = package_dir(state, package);
    fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("create registry dir {}", dir.display()))?;

    let final_path = tarball_path(state, package, filename);
    let tmp_path = final_path.with_extension("tgz.tmp");
    fs::write(&tmp_path, &bytes)
        .await
        .with_context(|| format!("write tmp tarball {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &final_path)
        .await
        .with_context(|| format!("rename tarball into {}", final_path.display()))?;

    let integrity = integrity(&bytes);

    // Best-effort: fire-and-forget upload to the cold tier. On failure we
    // leave `s3_uploaded = 0` and the next install will keep working from FS.
    if let Some(tier) = load_cold_tier(state).await? {
        let pkg = package.to_string();
        let fname = filename.to_string();
        let state_cl = state.clone();
        let body = bytes;
        tokio::spawn(async move {
            if let Err(e) = upload_to_cold(&tier, &pkg, &fname, body).await {
                tracing::warn!("registry: cold-tier upload failed for {pkg}/{fname}: {e:#}");
                return;
            }
            // Best-effort flag flip.
            if let Ok(db) = state_cl.db.lock() {
                let version = fname.strip_suffix(".tgz").and_then(|s| {
                    let unscoped = pkg.rsplit_once('/').map(|(_, n)| n).unwrap_or(pkg.as_str());
                    s.strip_prefix(&format!("{unscoped}-"))
                });
                if let Some(v) = version {
                    let _ = registry::db::mark_s3_uploaded(&db, &pkg, v);
                }
            }
        });
    }

    Ok(integrity)
}

/// Open a tarball for streaming, falling back to the cold tier if the hot
/// tier is empty (e.g. fresh VPS reinstall). Returns the open file handle plus
/// its size — callers wrap the file in a `ReaderStream` and feed it to
/// `Body::from_stream`, so the bytes never need to be materialised in RAM.
///
/// The S3 fallback path still buffers in memory (it's a rare miss after a VPS
/// reinstall and the bytes have to land on the local FS to satisfy subsequent
/// reads anyway). The hot path — which is the only one that runs under load —
/// is fully streamed.
pub async fn open_tarball_stream(
    state: &SharedState,
    package: &str,
    filename: &str,
) -> Result<(fs::File, u64)> {
    let path = tarball_path(state, package, filename);
    match fs::File::open(&path).await {
        Ok(file) => {
            let size = file.metadata().await?.len();
            Ok((file, size))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("registry: hot-tier miss for {package}/{filename}, trying S3");
            let tier = load_cold_tier(state)
                .await?
                .ok_or_else(|| anyhow!("tarball not found and no cold-tier configured"))?;
            let bytes = download_from_cold(&tier, package, filename).await?;
            // Write back to hot tier so subsequent reads are fast.
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent).await;
            }
            fs::write(&path, &bytes)
                .await
                .with_context(|| format!("write back hot-tier {}", path.display()))?;
            let file = fs::File::open(&path).await?;
            let size = file.metadata().await?.len();
            Ok((file, size))
        }
        Err(e) => Err(e.into()),
    }
}

/// Drop a tarball from the hot tier. Cold-tier blobs are left in place
/// (cleanup is a separate operator-driven concern).
pub async fn delete_tarball(state: &SharedState, package: &str, filename: &str) -> Result<()> {
    let path = tarball_path(state, package, filename);
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Pull the configured S3 storage row, decrypt creds, and decide whether
/// we have a usable cold tier. Returns `None` if the operator hasn't picked
/// a registry storage in /packages → "Configure S3" — registry still works,
/// just without S3 mirroring.
async fn load_cold_tier(state: &SharedState) -> Result<Option<ColdTier>> {
    let row = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow!("DB lock poisoned: {e}"))?;

        let storage_id: Option<String> = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'registry.s3_storage_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .filter(|s| !s.is_empty());

        let Some(id) = storage_id else {
            return Ok(None);
        };

        db.query_row(
            "SELECT storage_type, endpoint, region, bucket, access_key, secret_key, key_prefix
             FROM s3_storages
             WHERE id = ?1",
            [&id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?
    };

    let Some((storage_type, endpoint, region, bucket, access_key_enc, secret_key_enc, key_prefix)) =
        row
    else {
        return Ok(None);
    };

    let key = crate::crypto::get_secret_key();
    let access_key = crate::crypto::decrypt(&access_key_enc, &key)
        .map_err(|e| anyhow!("decrypt s3 access_key: {e}"))?;
    let secret_key = crate::crypto::decrypt(&secret_key_enc, &key)
        .map_err(|e| anyhow!("decrypt s3 secret_key: {e}"))?;

    Ok(Some(ColdTier {
        storage_type,
        endpoint,
        region,
        bucket,
        access_key,
        secret_key,
        key_prefix,
    }))
}

async fn upload_to_cold(
    tier: &ColdTier,
    package: &str,
    filename: &str,
    body: Vec<u8>,
) -> Result<()> {
    let key = registry::s3_key(&tier.key_prefix, package, filename);
    if tier.storage_type == "bunny" {
        // Bunny uses an HTTP PUT to the storage zone — see s3::bunny module.
        s3::bunny::upload_file(&tier.bucket, &tier.access_key, &tier.endpoint, &key, body).await
    } else {
        let client = s3::build_client(
            &tier.endpoint,
            &tier.region,
            &tier.access_key,
            &tier.secret_key,
        )?;
        s3::upload_file(&client, &tier.bucket, &key, body).await
    }
}

async fn download_from_cold(tier: &ColdTier, package: &str, filename: &str) -> Result<Vec<u8>> {
    let key = registry::s3_key(&tier.key_prefix, package, filename);
    if tier.storage_type == "bunny" {
        s3::bunny::download_file(&tier.bucket, &tier.access_key, &tier.endpoint, &key).await
    } else {
        let client = s3::build_client(
            &tier.endpoint,
            &tier.region,
            &tier.access_key,
            &tier.secret_key,
        )?;
        let resp = client
            .get_object()
            .bucket(&tier.bucket)
            .key(&key)
            .send()
            .await?;
        let bytes = resp.body.collect().await?.into_bytes().to_vec();
        Ok(bytes)
    }
}

/// On startup, drop any orphan tarballs on the hot tier that don't have a
/// matching `npm_versions` row — these are left over from a publish that
/// crashed between the FS write and the DB insert.
pub async fn gc_orphans(state: &SharedState) -> Result<()> {
    let root = registry::fs_root(&state.config);
    if !root.exists() {
        return Ok(());
    }
    let mut entries = fs::read_dir(&root).await?;
    while let Some(entry) = entries.next_entry().await? {
        let pkg_dir = entry.path();
        if !pkg_dir.is_dir() {
            continue;
        }
        let package_name = pkg_dir
            .file_name()
            .and_then(|s| s.to_str())
            .map(String::from);
        let Some(package_name) = package_name else {
            continue;
        };
        gc_package_dir(state, &pkg_dir, &package_name).await.ok();
    }
    Ok(())
}

async fn gc_package_dir(state: &SharedState, dir: &Path, package: &str) -> Result<()> {
    let mut files = fs::read_dir(dir).await?;
    while let Some(file) = files.next_entry().await? {
        let path = file.path();
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.ends_with(".tgz") {
            continue;
        }
        let unscoped = package.rsplit_once('/').map(|(_, n)| n).unwrap_or(package);
        let version = match name
            .strip_suffix(".tgz")
            .and_then(|s| s.strip_prefix(&format!("{unscoped}-")))
        {
            Some(v) => v.to_string(),
            None => continue,
        };

        let exists = {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow!("DB lock poisoned: {e}"))?;
            db.query_row(
                "SELECT 1 FROM npm_versions WHERE package_name = ?1 AND version = ?2",
                rusqlite::params![package, version],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false)
        };
        if !exists {
            tracing::warn!("registry: dropping orphan tarball {}", path.display());
            let _ = fs::remove_file(&path).await;
        }
    }
    Ok(())
}
