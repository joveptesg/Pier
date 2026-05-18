//! Build a tar.gz fixture and publish it via the CouchDB-style PUT.
//!
//! Pier's `_attachments` parser accepts both the unscoped (`name-ver.tgz`)
//! and full-scoped (`@scope/name-ver.tgz`) attachment keys — we use the
//! same form npm CLI sends for each package type so the harness exercises
//! the real production wire format.

use anyhow::{Context, Result};
use base64::Engine;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;
use sha2::{Digest, Sha512};

/// Build a deterministic tar.gz with package.json + index.js for `name@version`.
pub fn build_tarball(name: &str, version: &str) -> Result<Vec<u8>> {
    let pkg_json = format!(
        r#"{{"name":"{name}","version":"{version}","description":"pier-tests fixture","main":"index.js","license":"MIT"}}"#
    );
    let index_js = format!("module.exports = {{ v: \"{version}\" }};\n");

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    {
        let mut tar = tar::Builder::new(&mut gz);
        write_tar_entry(&mut tar, "package/package.json", pkg_json.as_bytes())?;
        write_tar_entry(&mut tar, "package/index.js", index_js.as_bytes())?;
        tar.finish().context("finalising tar")?;
    }
    gz.finish().context("finalising gzip")
}

fn write_tar_entry<W: std::io::Write>(
    tar: &mut tar::Builder<W>,
    path: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(1_700_000_000);
    header.set_cksum();
    tar.append_data(&mut header, path, data)
        .with_context(|| format!("tar append {path}"))?;
    Ok(())
}

/// Build the JSON body of a publish PUT for `name@version`, embedding the
/// tarball as a base64 `_attachments` entry. Matches the canonical npm CLI
/// shape (used both for scoped and unscoped packages — the attachment key
/// is the only diff and Pier accepts both forms).
pub fn build_publish_body(name: &str, version: &str, tarball: &[u8]) -> serde_json::Value {
    let integrity = compute_integrity(tarball);
    let attachment_name = format!("{name}-{version}.tgz");
    let basename_only = attachment_name
        .rsplit_once('/')
        .map(|(_, b)| b.to_string())
        .unwrap_or_else(|| attachment_name.clone());

    json!({
        "_id": name,
        "name": name,
        "description": "pier-tests fixture",
        "dist-tags": { "latest": version },
        "versions": {
            version: {
                "name": name,
                "version": version,
                "description": "pier-tests fixture",
                "main": "index.js",
                "license": "MIT",
                "dist": { "integrity": integrity }
            }
        },
        "_attachments": {
            basename_only: {
                "content_type": "application/octet-stream",
                "data": base64::engine::general_purpose::STANDARD.encode(tarball),
                "length": tarball.len()
            }
        }
    })
}

fn compute_integrity(bytes: &[u8]) -> String {
    let digest = Sha512::digest(bytes);
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// PUT the publish body and return the HTTP status. The harness uses this to
/// pre-seed fixtures for packument/dist-tag/etc scenarios; install matrix
/// (P3) shells out to real clients instead.
pub async fn publish(
    http: &reqwest::Client,
    registry_url: &str,
    token: &str,
    name: &str,
    version: &str,
) -> Result<reqwest::StatusCode> {
    let tarball = build_tarball(name, version)?;
    let body = build_publish_body(name, version, &tarball);
    let url = format!("{}{}", registry_url, urlencoding::encode(name));
    let r = http
        .put(&url)
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
        .context("PUT publish")?;
    Ok(r.status())
}
