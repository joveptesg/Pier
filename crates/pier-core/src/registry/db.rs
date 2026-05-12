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
    /// True when the package was unpublished (tombstone row kept around).
    pub unpublished: bool,
    /// Number of versions with `deprecated` set on their manifest. > 0 lights
    /// up a badge in the UI.
    pub deprecated_count: i64,
}

/// Look up a package + all its versions. Returns None if unknown OR if the
/// package has been unpublished (we keep the row as a tombstone so a
/// re-publish under the same name can be rejected per npm policy, but reads
/// should see the package as gone).
pub fn load_packument(conn: &Connection, name: &str) -> Result<Option<Packument>> {
    let pkg_row: Option<(String, String, i64, Option<i64>)> = conn
        .query_row(
            "SELECT description, dist_tags_json, is_proxy, unpublished_at
             FROM npm_packages WHERE name = ?1",
            [name],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()?;

    let Some((description, dist_tags_json, is_proxy, unpublished_at)) = pkg_row else {
        return Ok(None);
    };
    if unpublished_at.is_some() {
        return Ok(None);
    }

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
///
/// Returns None for unpublished packages — `npm_packages.unpublished_at` is
/// the canonical tombstone, so checking it here keeps version reads consistent
/// with `load_packument`.
pub fn load_version_manifest(
    conn: &Connection,
    name: &str,
    version: &str,
) -> Result<Option<serde_json::Value>> {
    if is_unpublished(conn, name)? {
        return Ok(None);
    }
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

/// Whether a package has been unpublished. Cheap — single-row lookup.
pub fn is_unpublished(conn: &Connection, name: &str) -> Result<bool> {
    let ts: Option<i64> = conn
        .query_row(
            "SELECT unpublished_at FROM npm_packages WHERE name = ?1",
            [name],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()?
        .flatten();
    Ok(ts.is_some())
}

/// Lookup tarball metadata by package + filename. Used by the GET tarball
/// handler to verify the file exists in our index before reading from FS/S3.
/// Returns None for unpublished packages so 404 is consistent across reads.
pub fn lookup_tarball(
    conn: &Connection,
    package: &str,
    version: &str,
) -> Result<Option<TarballMeta>> {
    if is_unpublished(conn, package)? {
        return Ok(None);
    }
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

    // Upsert package, merging dist-tags (new tags win). A re-publish after
    // `npm unpublish` should resurrect the package — clear `unpublished_at`
    // so reads see the new version.
    tx.execute(
        "INSERT INTO npm_packages
            (name, description, dist_tags_json, is_proxy, created_at, updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?4)
         ON CONFLICT(name) DO UPDATE SET
            description = COALESCE(NULLIF(excluded.description, ''), npm_packages.description),
            dist_tags_json = ?3,
            unpublished_at = NULL,
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
                p.unpublished_at,
                COALESCE(SUM(v.tarball_size), 0) AS total_size,
                COUNT(v.version) AS version_count,
                COALESCE(SUM(
                    CASE WHEN json_extract(v.manifest_json, '$.deprecated') IS NOT NULL
                         THEN 1 ELSE 0 END
                ), 0) AS deprecated_count
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
                unpublished: row.get::<_, Option<i64>>(5)?.is_some(),
                total_size: row.get(6)?,
                version_count: row.get(7)?,
                deprecated_count: row.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Pull the README of the `latest` version, if any. Looked up via the dist-tag
/// rather than max-version because semver-naive max is unreliable for pre-release
/// tags. Returns the raw markdown — UI is in charge of rendering and escaping.
pub fn load_readme(conn: &Connection, package: &str) -> Result<Option<String>> {
    if is_unpublished(conn, package)? {
        return Ok(None);
    }
    let row: Option<String> = conn
        .query_row(
            "SELECT json_extract(v.manifest_json, '$.readme')
             FROM npm_versions v
             JOIN npm_packages p ON p.name = v.package_name
             WHERE v.package_name = ?1
               AND v.version = json_extract(p.dist_tags_json, '$.latest')",
            [package],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(row.filter(|s| !s.is_empty()))
}

/// `VersionSummary` enriched with the deprecation flag for the detail page.
#[derive(Debug, Clone, Serialize)]
pub struct VersionListing {
    pub version: String,
    pub size: i64,
    pub sha512: String,
    pub published_by: Option<String>,
    pub published_at: i64,
    pub s3_uploaded: bool,
    pub deprecated: Option<String>,
}

/// Per-version listing for the package detail page, with deprecation status.
pub fn list_versions_with_deprecation(
    conn: &Connection,
    package: &str,
) -> Result<Vec<VersionListing>> {
    let mut stmt = conn.prepare(
        "SELECT version, tarball_size, tarball_sha512, published_by, published_at,
                s3_uploaded, json_extract(manifest_json, '$.deprecated')
         FROM npm_versions WHERE package_name = ?1
         ORDER BY published_at DESC",
    )?;
    let rows = stmt
        .query_map([package], |row| {
            Ok(VersionListing {
                version: row.get(0)?,
                size: row.get(1)?,
                sha512: row.get(2)?,
                published_by: row.get(3)?,
                published_at: row.get(4)?,
                s3_uploaded: row.get::<_, i64>(5)? != 0,
                deprecated: row.get::<_, Option<String>>(6)?,
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

/// Outcome of a destructive operation — returned to the handler so it can drop
/// the tarball blob from FS after the DB commit.
///
/// `version` and `published_by` aren't consumed today (the handler only needs
/// `package`/`filename` to call `storage::delete_tarball`) but they're cheap
/// to surface and the audit-log work in PR 5 will read them.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RemovedTarball {
    pub package: String,
    pub version: String,
    pub filename: String,
    pub published_by: Option<String>,
}

/// Read the dist-tags map for a package.
pub fn load_dist_tags(
    conn: &Connection,
    package: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    if is_unpublished(conn, package)? {
        return Ok(None);
    }
    let row: Option<String> = conn
        .query_row(
            "SELECT dist_tags_json FROM npm_packages WHERE name = ?1",
            [package],
            |row| row.get(0),
        )
        .optional()?;
    Ok(row.map(|s| serde_json::from_str(&s).unwrap_or_default()))
}

/// Set or replace a single dist-tag. Returns Ok(()) on success, or BadRequest
/// (via anyhow) if `version` is not actually published. Wrapped in a
/// transaction so a concurrent publish can't race the tag update.
pub fn set_dist_tag(conn: &Connection, package: &str, tag: &str, version: &str) -> Result<()> {
    if tag.is_empty() || tag.contains('/') || tag.contains(' ') {
        anyhow::bail!("invalid dist-tag name");
    }
    let tx = conn.unchecked_transaction()?;

    // Refuse to point a tag at a version that doesn't exist.
    let exists: bool = tx
        .query_row(
            "SELECT 1 FROM npm_versions
             WHERE package_name = ?1 AND version = ?2",
            params![package, version],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !exists {
        anyhow::bail!("version {package}@{version} does not exist");
    }

    let current: String = tx
        .query_row(
            "SELECT dist_tags_json FROM npm_packages WHERE name = ?1",
            [package],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| anyhow::anyhow!("package not found"))?;

    let mut tags: BTreeMap<String, String> = serde_json::from_str(&current).unwrap_or_default();
    tags.insert(tag.to_string(), version.to_string());
    let new_json = serde_json::to_string(&tags)?;
    let now = chrono::Utc::now().timestamp();
    tx.execute(
        "UPDATE npm_packages SET dist_tags_json = ?1, updated_at = ?2 WHERE name = ?3",
        params![new_json, now, package],
    )?;
    tx.commit()?;
    Ok(())
}

/// Remove a dist-tag. Removing `latest` is rejected (npm refuses too) — it's
/// the only required tag and every install path falls back to it.
pub fn remove_dist_tag(conn: &Connection, package: &str, tag: &str) -> Result<bool> {
    if tag == "latest" {
        anyhow::bail!("refusing to remove the 'latest' dist-tag");
    }
    let tx = conn.unchecked_transaction()?;
    let current: Option<String> = tx
        .query_row(
            "SELECT dist_tags_json FROM npm_packages WHERE name = ?1",
            [package],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(current) = current else {
        return Ok(false);
    };
    let mut tags: BTreeMap<String, String> = serde_json::from_str(&current).unwrap_or_default();
    let removed = tags.remove(tag).is_some();
    if removed {
        let new_json = serde_json::to_string(&tags)?;
        let now = chrono::Utc::now().timestamp();
        tx.execute(
            "UPDATE npm_packages SET dist_tags_json = ?1, updated_at = ?2 WHERE name = ?3",
            params![new_json, now, package],
        )?;
    }
    tx.commit()?;
    Ok(removed)
}

/// Delete a single version. If the version was the target of `latest`, the
/// tag is re-pointed at the highest remaining version (semver-naive: lexical
/// max — good enough for the common monotonic-versioning case; consumers who
/// need true semver ordering can re-tag explicitly afterwards). Returns the
/// tarball metadata so the caller can drop the blob from FS.
pub fn delete_version(
    conn: &Connection,
    package: &str,
    version: &str,
) -> Result<Option<RemovedTarball>> {
    let tx = conn.unchecked_transaction()?;

    let published_by: Option<String> = tx
        .query_row(
            "SELECT published_by FROM npm_versions
             WHERE package_name = ?1 AND version = ?2",
            params![package, version],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    let deleted = tx.execute(
        "DELETE FROM npm_versions WHERE package_name = ?1 AND version = ?2",
        params![package, version],
    )?;
    if deleted == 0 {
        return Ok(None);
    }

    // Re-target dist-tags that pointed at the deleted version.
    let current: String = tx
        .query_row(
            "SELECT dist_tags_json FROM npm_packages WHERE name = ?1",
            [package],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default();
    let mut tags: BTreeMap<String, String> = serde_json::from_str(&current).unwrap_or_default();
    let pointed_at_deleted: Vec<String> = tags
        .iter()
        .filter(|(_, v)| v.as_str() == version)
        .map(|(k, _)| k.clone())
        .collect();

    if !pointed_at_deleted.is_empty() {
        // Find the new highest version (lexical max — see fn doc).
        let new_latest: Option<String> = tx
            .query_row(
                "SELECT MAX(version) FROM npm_versions WHERE package_name = ?1",
                [package],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        for tag in pointed_at_deleted {
            match &new_latest {
                Some(v) => {
                    tags.insert(tag, v.clone());
                }
                None => {
                    tags.remove(&tag);
                }
            }
        }
        let new_json = serde_json::to_string(&tags)?;
        let now = chrono::Utc::now().timestamp();
        tx.execute(
            "UPDATE npm_packages SET dist_tags_json = ?1, updated_at = ?2 WHERE name = ?3",
            params![new_json, now, package],
        )?;
    }

    tx.commit()?;

    Ok(Some(RemovedTarball {
        package: package.to_string(),
        version: version.to_string(),
        filename: crate::registry::tarball_filename(package, version),
        published_by,
    }))
}

/// Tombstone a package: drop every version and set `unpublished_at`. Returns
/// the list of removed tarballs so the caller can drop blobs from FS.
pub fn delete_package(conn: &Connection, package: &str) -> Result<Vec<RemovedTarball>> {
    let tx = conn.unchecked_transaction()?;

    let mut stmt =
        tx.prepare("SELECT version, published_by FROM npm_versions WHERE package_name = ?1")?;
    let removed: Vec<RemovedTarball> = stmt
        .query_map([package], |row| {
            let version: String = row.get(0)?;
            let published_by: Option<String> = row.get(1)?;
            Ok(RemovedTarball {
                package: package.to_string(),
                filename: crate::registry::tarball_filename(package, &version),
                version,
                published_by,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    tx.execute(
        "DELETE FROM npm_versions WHERE package_name = ?1",
        [package],
    )?;
    let now = chrono::Utc::now().timestamp();
    tx.execute(
        "UPDATE npm_packages SET unpublished_at = ?1, dist_tags_json = '{}', updated_at = ?1
         WHERE name = ?2",
        params![now, package],
    )?;

    tx.commit()?;
    Ok(removed)
}

/// Mark one or more versions as deprecated. `messages` maps version → message.
/// Empty message clears the deprecation. Patches `manifest_json.deprecated`
/// in place via `json_set` so the abbreviated packument picks it up
/// automatically.
pub fn deprecate_versions(
    conn: &Connection,
    package: &str,
    messages: &BTreeMap<String, String>,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    for (version, msg) in messages {
        if msg.is_empty() {
            tx.execute(
                "UPDATE npm_versions
                 SET manifest_json = json_remove(manifest_json, '$.deprecated')
                 WHERE package_name = ?1 AND version = ?2",
                params![package, version],
            )?;
        } else {
            tx.execute(
                "UPDATE npm_versions
                 SET manifest_json = json_set(manifest_json, '$.deprecated', ?3)
                 WHERE package_name = ?1 AND version = ?2",
                params![package, version, msg],
            )?;
        }
    }
    let now = chrono::Utc::now().timestamp();
    tx.execute(
        "UPDATE npm_packages SET updated_at = ?1 WHERE name = ?2",
        params![now, package],
    )?;
    tx.commit()?;
    Ok(())
}
