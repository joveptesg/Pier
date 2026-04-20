//! Registry credentials lookup for Docker operations.
//!
//! Storage: `registry_credentials` table. Each row is keyed by
//! `(project_id, registry)` where `project_id = NULL` means "global".
//! Lookups prefer project-specific rows, then fall back to global.

use std::collections::HashMap;

use bollard::auth::DockerCredentials;
use rusqlite::{Connection, OptionalExtension};

use crate::crypto;
use crate::error::{AppError, AppResult};

const DEFAULT_REGISTRY: &str = "docker.io";

/// Extract the registry host from a fully-qualified image reference.
///
/// Handles: plain tags, digests, ports, and default Docker Hub fallback.
pub fn parse_registry_host(image: &str) -> String {
    let trimmed = image.trim();
    let first_slash = match trimmed.find('/') {
        Some(idx) => idx,
        None => return DEFAULT_REGISTRY.into(),
    };
    let candidate = &trimmed[..first_slash];
    if candidate == "localhost" || candidate.contains('.') || candidate.contains(':') {
        candidate.to_string()
    } else {
        DEFAULT_REGISTRY.into()
    }
}

fn row_to_credentials(
    username: String,
    password_enc: String,
    registry: String,
) -> AppResult<DockerCredentials> {
    let key = crypto::get_secret_key();
    let password = crypto::decrypt(&password_enc, &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("decrypt creds: {e}")))?;
    Ok(DockerCredentials {
        username: Some(username),
        password: Some(password),
        serveraddress: Some(registry),
        ..Default::default()
    })
}

/// Look up credentials for a single image.
///
/// Order: project-specific row, then global row (`project_id IS NULL`).
/// Returns `None` when neither exists — callers pass `None` through to Bollard,
/// which falls back to the Docker daemon's own config.
///
/// Currently only reachable via tests; all compose/build paths use
/// [`auth_map_for_service`]. Public API retained for single-image `create_image`
/// callers (e.g., future Bollard-driven pulls outside the compose pipeline).
#[allow(dead_code)]
pub fn credentials_for(
    db: &Connection,
    project_id: Option<&str>,
    image: &str,
) -> AppResult<Option<DockerCredentials>> {
    let host = parse_registry_host(image);
    let row = db
        .query_row(
            r#"
            SELECT username, password_enc, registry
            FROM registry_credentials
            WHERE registry = ?1
              AND (project_id = ?2 OR project_id IS NULL)
            ORDER BY (project_id IS NULL) ASC
            LIMIT 1
            "#,
            rusqlite::params![host, project_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;

    match row {
        Some((username, password_enc, registry)) => {
            Ok(Some(row_to_credentials(username, password_enc, registry)?))
        }
        None => Ok(None),
    }
}

/// Return all credentials relevant to a project as a `host -> DockerCredentials` map.
///
/// Used by `docker build_image` (multi-stage FROM may cross registries) and by
/// the compose CLI wrapper (it writes a temporary `~/.docker/config.json`).
/// Project-specific rows override global ones for the same host.
pub fn all_credentials_for_project(
    db: &Connection,
    project_id: Option<&str>,
) -> AppResult<HashMap<String, DockerCredentials>> {
    let mut stmt = db.prepare(
        r#"
        SELECT registry, username, password_enc, project_id
        FROM registry_credentials
        WHERE project_id = ?1 OR project_id IS NULL
        "#,
    )?;

    let rows = stmt
        .query_map(rusqlite::params![project_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Project-specific rows win over global on the same host.
    let mut map: HashMap<String, (DockerCredentials, bool)> = HashMap::new();
    for (registry, username, password_enc, row_project_id) in rows {
        let is_project_specific = row_project_id.is_some();
        let creds = row_to_credentials(username, password_enc, registry.clone())?;
        map.entry(registry)
            .and_modify(|slot| {
                if is_project_specific && !slot.1 {
                    *slot = (creds.clone(), true);
                }
            })
            .or_insert((creds, is_project_specific));
    }

    Ok(map.into_iter().map(|(k, (v, _))| (k, v)).collect())
}

/// Convenience: `project_id` + creds for a service in one call.
///
/// Used by deploy pipelines that only hold a `service_id`. Returns an empty map
/// when the service has no registered credentials so callers can pass it
/// straight into [`write_docker_config`] without pre-checks.
pub fn auth_map_for_service(
    db: &Connection,
    service_id: &str,
) -> AppResult<HashMap<String, DockerCredentials>> {
    let project_id: Option<String> = db
        .query_row(
            "SELECT project_id FROM services WHERE id = ?1",
            [service_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    all_credentials_for_project(db, project_id.as_deref())
}

/// Write a minimal `config.json` holding the given auths into a fresh temp dir.
///
/// Returns `Ok(None)` when there are no credentials — callers then skip setting
/// `DOCKER_CONFIG` so the `docker compose` CLI keeps using the daemon's default
/// config. The `TempDir` must be kept alive until the CLI invocation finishes;
/// its Drop cleans up the on-disk file.
pub fn write_docker_config(
    auth_map: &HashMap<String, DockerCredentials>,
) -> anyhow::Result<Option<tempfile::TempDir>> {
    use base64::{engine::general_purpose::STANDARD as B64, Engine};

    if auth_map.is_empty() {
        return Ok(None);
    }

    let dir = tempfile::tempdir()?;
    let mut auths = serde_json::Map::new();
    for (host, creds) in auth_map {
        let user = creds.username.as_deref().unwrap_or("");
        let pass = creds.password.as_deref().unwrap_or("");
        let encoded = B64.encode(format!("{user}:{pass}"));
        auths.insert(host.clone(), serde_json::json!({ "auth": encoded }));
    }
    let config = serde_json::json!({ "auths": auths });
    std::fs::write(
        dir.path().join("config.json"),
        serde_json::to_string(&config)?,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            dir.path().join("config.json"),
            std::fs::Permissions::from_mode(0o600),
        );
    }

    Ok(Some(dir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hub_shortcut() {
        assert_eq!(parse_registry_host("alpine"), "docker.io");
        assert_eq!(parse_registry_host("library/alpine"), "docker.io");
        assert_eq!(parse_registry_host("user/repo:tag"), "docker.io");
        assert_eq!(parse_registry_host("user/repo@sha256:abc"), "docker.io");
    }

    #[test]
    fn parse_ghcr_and_gitlab() {
        assert_eq!(parse_registry_host("ghcr.io/org/api:v1"), "ghcr.io");
        assert_eq!(
            parse_registry_host("registry.gitlab.com:5050/g/api:tag"),
            "registry.gitlab.com:5050"
        );
        assert_eq!(parse_registry_host("localhost/img"), "localhost");
        assert_eq!(parse_registry_host("localhost:5000/img"), "localhost:5000");
    }

    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE registry_credentials (
                id TEXT PRIMARY KEY NOT NULL,
                project_id TEXT,
                registry TEXT NOT NULL,
                username TEXT NOT NULL,
                password_enc TEXT NOT NULL,
                label TEXT,
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            "#,
        )
        .unwrap();
        conn
    }

    fn insert(conn: &Connection, id: &str, project: Option<&str>, registry: &str, user: &str) {
        let key = crypto::get_secret_key();
        let enc = crypto::encrypt("secret-pass", &key).unwrap();
        conn.execute(
            "INSERT INTO registry_credentials (id, project_id, registry, username, password_enc)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![id, project, registry, user, enc],
        )
        .unwrap();
    }

    #[test]
    fn project_overrides_global() {
        let db = setup_db();
        insert(&db, "g1", None, "ghcr.io", "global-user");
        insert(&db, "p1", Some("proj-a"), "ghcr.io", "project-user");

        let creds = credentials_for(&db, Some("proj-a"), "ghcr.io/x/y:v1")
            .unwrap()
            .expect("creds for project");
        assert_eq!(creds.username.as_deref(), Some("project-user"));

        let global = credentials_for(&db, Some("proj-b"), "ghcr.io/x/y:v1")
            .unwrap()
            .expect("creds fallback to global");
        assert_eq!(global.username.as_deref(), Some("global-user"));
    }

    #[test]
    fn missing_host_returns_none() {
        let db = setup_db();
        insert(&db, "g1", None, "ghcr.io", "u");
        let res = credentials_for(&db, None, "docker.io/lib/alpine").unwrap();
        assert!(res.is_none());
    }

    #[test]
    fn all_credentials_merge_with_project_override() {
        let db = setup_db();
        insert(&db, "g-ghcr", None, "ghcr.io", "g-ghcr-user");
        insert(&db, "g-hub", None, "docker.io", "g-hub-user");
        insert(&db, "p-ghcr", Some("proj-a"), "ghcr.io", "p-ghcr-user");

        let map = all_credentials_for_project(&db, Some("proj-a")).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map["ghcr.io"].username.as_deref(), Some("p-ghcr-user"));
        assert_eq!(map["docker.io"].username.as_deref(), Some("g-hub-user"));
    }
}
