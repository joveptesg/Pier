//! SQLite helpers for the embedded npm registry.
//!
//! Two tables are at play (see migration #34):
//! - `npm_packages` — one row per package, with the canonical `dist-tags` map.
//! - `npm_versions` — one row per (package, version), holding the published
//!   manifest plus the integrity hash.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Materialised `npm_packages` row plus its versions, ready to render as a
/// packument response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Packument {
    pub name: String,
    pub description: String,
    pub dist_tags: BTreeMap<String, String>,
    pub is_proxy: bool,
    /// version → manifest_json (already serialised — kept as raw JSON to avoid
    /// double parse/serialize round-trips on the hot read path).
    pub versions: BTreeMap<String, serde_json::Value>,
    pub time: BTreeMap<String, String>,
}

/// Per-version row used by the UI listing.
#[derive(Debug, Clone, Serialize)]
pub struct VersionSummary {
    pub version: String,
    pub size: i64,
    pub sha512: String,
    pub published_by: Option<String>,
    pub published_at: i64,
    pub s3_uploaded: bool,
}

/// Lightweight package row for the listing UI.
#[derive(Debug, Clone, Serialize)]
pub struct PackageSummary {
    pub name: String,
    pub description: String,
    pub latest_version: Option<String>,
    pub version_count: i64,
    pub total_size: i64,
    pub is_proxy: bool,
    pub updated_at: i64,
}

/// Look up a package + all its versions. Returns None if unknown.
pub fn load_packument(conn: &Connection, name: &str) -> Result<Option<Packument>> {
    let pkg_row: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT description, dist_tags_json, is_proxy
             FROM npm_packages WHERE name = ?1",
            [name],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;

    let Some((description, dist_tags_json, is_proxy)) = pkg_row else {
        return Ok(None);
    };

    let dist_tags: BTreeMap<String, String> =
        serde_json::from_str(&dist_tags_json).unwrap_or_default();

    let mut versions = BTreeMap::new();
    let mut time = BTreeMap::new();

    let mut stmt = conn.prepare(
        "SELECT version, manifest_json, published_at
         FROM npm_versions WHERE package_name = ?1
         ORDER BY published_at ASC",
    )?;
    let mut rows = stmt.query([name])?;
    while let Some(row) = rows.next()? {
        let v: String = row.get(0)?;
        let manifest: String = row.get(1)?;
        let published_at: i64 = row.get(2)?;
        let manifest_val: serde_json::Value =
            serde_json::from_str(&manifest).unwrap_or(serde_json::Value::Null);
        versions.insert(v.clone(), manifest_val);
        time.insert(v, ts_to_iso(published_at));
    }

    Ok(Some(Packument {
        name: name.to_string(),
        description,
        dist_tags,
        is_proxy: is_proxy != 0,
        versions,
        time,
    }))
}

/// Single-version manifest fetch (for `npm view <pkg>@<ver>`).
pub fn load_version_manifest(
    conn: &Connection,
    name: &str,
    version: &str,
) -> Result<Option<serde_json::Value>> {
    let row: Option<String> = conn
        .query_row(
            "SELECT manifest_json FROM npm_versions
             WHERE package_name = ?1 AND version = ?2",
            params![name, version],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.and_then(|m| serde_json::from_str(&m).ok()))
}

/// Lookup tarball metadata by package + filename. Used by the GET tarball
/// handler to verify the file exists in our index before reading from FS/S3.
pub fn lookup_tarball(
    conn: &Connection,
    package: &str,
    version: &str,
) -> Result<Option<TarballMeta>> {
    let row: Option<(i64, String, i64)> = conn
        .query_row(
            "SELECT tarball_size, tarball_sha512, s3_uploaded
             FROM npm_versions WHERE package_name = ?1 AND version = ?2",
            params![package, version],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?;

    Ok(row.map(|(size, sha, uploaded)| TarballMeta {
        size,
        sha512: sha,
        s3_uploaded: uploaded != 0,
    }))
}

/// Existence-check result for a tarball lookup. Fields are kept around for
/// cache-header rendering (Content-Length, ETag) once that's wired up;
/// today the handler only checks the `Option`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct TarballMeta {
    pub size: i64,
    pub sha512: String,
    pub s3_uploaded: bool,
}

/// Insert (or fail with a UNIQUE-violation 409) a freshly published version.
/// Caller must have already validated and persisted the tarball blob.
#[allow(clippy::too_many_arguments)]
pub fn insert_version(
    conn: &Connection,
    package: &str,
    description: &str,
    version: &str,
    manifest_json: &str,
    tarball_size: i64,
    tarball_sha512: &str,
    published_by: Option<&str>,
    new_dist_tags: &BTreeMap<String, String>,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    let dist_tags_json = serde_json::to_string(new_dist_tags)?;

    let tx = conn.unchecked_transaction()?;

    // Upsert package, merging dist-tags (new tags win).
    tx.execute(
        "INSERT INTO npm_packages
            (name, description, dist_tags_json, is_proxy, created_at, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?4)
         ON CONFLICT(name) DO UPDATE SET
            description = COALESCE(NULLIF(excluded.description, ''), npm_packages.description),
            dist_tags_json = ?3,
            updated_at = ?4",
        params![package, description, dist_tags_json, now],
    )?;

    // Insert version — UNIQUE on (package_name, version) gives idempotency.
    tx.execute(
        "INSERT INTO npm_versions
            (package_name, version, manifest_json, tarball_size,
             tarball_sha512, s3_uploaded, published_by, published_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6, ?7)",
        params![
            package,
            version,
            manifest_json,
            tarball_size,
            tarball_sha512,
            published_by,
            now,
        ],
    )?;

    tx.commit()?;
    Ok(())
}

/// Mark a tarball as successfully uploaded to S3.
pub fn mark_s3_uploaded(conn: &Connection, package: &str, version: &str) -> Result<()> {
    conn.execute(
        "UPDATE npm_versions SET s3_uploaded = 1
         WHERE package_name = ?1 AND version = ?2",
        params![package, version],
    )?;
    Ok(())
}

/// Listing for the UI (`/registry/npm`).
pub fn list_packages(conn: &Connection, only_private: bool) -> Result<Vec<PackageSummary>> {
    let where_clause = if only_private {
        "WHERE p.is_proxy = 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT p.name, p.description, p.dist_tags_json, p.is_proxy, p.updated_at,
                COALESCE(SUM(v.tarball_size), 0) AS total_size,
                COUNT(v.version) AS version_count
         FROM npm_packages p
         LEFT JOIN npm_versions v ON v.package_name = p.name
         {where_clause}
         GROUP BY p.name
         ORDER BY p.updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |row| {
            let dist_tags_json: String = row.get(2)?;
            let dist_tags: BTreeMap<String, String> =
                serde_json::from_str(&dist_tags_json).unwrap_or_default();
            Ok(PackageSummary {
                name: row.get(0)?,
                description: row.get(1)?,
                latest_version: dist_tags.get("latest").cloned(),
                is_proxy: row.get::<_, i64>(3)? != 0,
                updated_at: row.get(4)?,
                total_size: row.get(5)?,
                version_count: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Per-version listing for the package detail page.
pub fn list_versions(conn: &Connection, package: &str) -> Result<Vec<VersionSummary>> {
    let mut stmt = conn.prepare(
        "SELECT version, tarball_size, tarball_sha512, published_by, published_at, s3_uploaded
         FROM npm_versions WHERE package_name = ?1
         ORDER BY published_at DESC",
    )?;
    let rows = stmt
        .query_map([package], |row| {
            Ok(VersionSummary {
                version: row.get(0)?,
                size: row.get(1)?,
                sha512: row.get(2)?,
                published_by: row.get(3)?,
                published_at: row.get(4)?,
                s3_uploaded: row.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Convert a unix timestamp to an ISO-8601 string for the npm `time` map.
fn ts_to_iso(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}
