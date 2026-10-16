//! Emergency export: env vars (+ local DB dumps) of one service or of every
//! service on a server, packed into an AES-256 password-protected zip that is
//! streamed straight to the browser.
//!
//! Nothing is kept on the server: the archive is built in an anonymous temp
//! file (`tempfile::tempfile_in` — unlinked on creation on Linux), and its
//! space is released as soon as the response body drops the handle.
//!
//! DB dumps reuse `backup::executor`, which drives the *local* docker CLI, so
//! only databases running on the panel host are dumped. Services on agents
//! still get their env exported; the manifest marks their dump as skipped.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::Serialize;
use zip::write::{FileOptions, ZipWriter};
use zip::{AesMode, CompressionMethod};

use crate::backup::executor::{self, DbCredential, PerDbDumpFormat};
use crate::state::SharedState;

pub enum Scope {
    Service(String),
    Server(String),
    Project(String),
    /// Settings → Export. `ids: None` = every project (new ones included).
    Projects {
        ids: Option<Vec<String>>,
        include_unassigned: bool,
    },
}

/// One `services` row with everything the export needs, secrets decrypted.
pub struct ExportService {
    pub id: String,
    pub name: String,
    pub project: Option<String>,
    pub server_id: Option<String>,
    pub server_name: Option<String>,
    /// Runs on the panel host — the only place we can `docker exec` a dump.
    pub is_local: bool,
    pub catalog_id: Option<String>,
    pub category: Option<String>,
    /// Full `env_json`, template vars (lowercase keys) included.
    pub env: BTreeMap<String, String>,
    /// `env_json` was non-empty but could not be decrypted/parsed.
    pub env_error: bool,
    pub build_env: Option<String>,
    pub credentials: Vec<DbCredential>,
}

#[derive(Serialize, Debug, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DumpStatus {
    /// Not a database we know how to dump.
    None,
    Skipped {
        reason: String,
    },
    Done {
        files: Vec<String>,
        errors: Vec<String>,
    },
}

pub struct ExportSummary {
    pub services: usize,
    pub dumps: usize,
}

/// Load the services covered by `scope`. Returns the scope's display name
/// (service, server or project name) alongside the rows; `None` if the target is gone.
pub fn load_services(
    state: &SharedState,
    scope: &Scope,
) -> Result<Option<(String, Vec<ExportService>)>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    const COLS: &str = "SELECT s.id, s.name, p.name, s.server_id, sv.name,
            (s.server_id IS NULL OR s.server_id = 'local' OR COALESCE(sv.is_local, 0) = 1),
            s.catalog_id, s.category, s.env_json, s.build_env_vars
         FROM services s
         LEFT JOIN projects p ON p.id = s.project_id
         LEFT JOIN servers sv ON sv.id = s.server_id";

    use rusqlite::types::Value;
    let (label, filter, params): (String, String, Vec<Value>) = match scope {
        Scope::Service(id) => {
            let Some(name) = db
                .query_row("SELECT name FROM services WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
            else {
                return Ok(None);
            };
            (name, "s.id = ?1".into(), vec![id.clone().into()])
        }
        Scope::Server(id) => {
            let Some((name, is_local)) = db
                .query_row(
                    "SELECT name, is_local FROM servers WHERE id = ?1",
                    [id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)),
                )
                .optional()?
            else {
                return Ok(None);
            };
            // Same rule as servers::get: on the local server, NULL/'local'
            // server_id rows belong to it too.
            (
                name,
                "s.server_id = ?1
                   OR (?2 = 1 AND (s.server_id IS NULL OR s.server_id = 'local'))"
                    .into(),
                vec![id.clone().into(), Value::Integer(is_local as i64)],
            )
        }
        Scope::Project(id) => {
            let Some(name) = db
                .query_row("SELECT name FROM projects WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .optional()?
            else {
                return Ok(None);
            };
            (name, "s.project_id = ?1".into(), vec![id.clone().into()])
        }
        Scope::Projects {
            ids,
            include_unassigned,
        } => {
            let mut parts = Vec::new();
            match ids {
                None => parts.push("s.project_id IS NOT NULL".to_string()),
                Some(ids) if !ids.is_empty() => {
                    let marks = (1..=ids.len())
                        .map(|i| format!("?{i}"))
                        .collect::<Vec<_>>()
                        .join(",");
                    parts.push(format!("s.project_id IN ({marks})"));
                }
                Some(_) => {}
            }
            if *include_unassigned {
                parts.push("s.project_id IS NULL".into());
            }
            if parts.is_empty() {
                parts.push("0".into());
            }
            let label = match ids {
                None => "all projects".to_string(),
                Some(ids) => format!("{} projects", ids.len()),
            };
            let params = ids
                .iter()
                .flatten()
                .map(|id| Value::from(id.clone()))
                .collect();
            (label, parts.join(" OR "), params)
        }
    };

    let sql = format!("{COLS} WHERE {filter} ORDER BY p.name, s.name");
    let mut stmt = db.prepare(&sql)?;
    let rows: Vec<_> = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, bool>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    drop(db);

    let mut services = Vec::with_capacity(rows.len());
    for (
        id,
        name,
        project,
        server_id,
        server_name,
        is_local,
        catalog_id,
        category,
        env_json,
        build_env,
    ) in rows
    {
        let (env, env_error) = decode_env(env_json.as_deref());
        let credentials = crate::backup::scheduler::load_db_credentials(state, &id)?;
        services.push(ExportService {
            id,
            name,
            project,
            server_id,
            server_name,
            is_local,
            catalog_id,
            category,
            env,
            env_error,
            build_env: build_env.filter(|b| !b.trim().is_empty()),
            credentials,
        });
    }
    Ok(Some((label, services)))
}

/// Decrypt `env_json`. `decrypt_env_json` swallows failures into `"{}"`; we
/// flag that case so the export says "env unreadable" instead of looking empty.
fn decode_env(stored: Option<&str>) -> (BTreeMap<String, String>, bool) {
    let decrypted = crate::crypto::decrypt_env_json(stored);
    let env: BTreeMap<String, String> = serde_json::from_str(&decrypted).unwrap_or_default();
    let had_data = matches!(stored, Some(s) if !s.trim().is_empty() && s != "null" && s != "{}");
    let error = env.is_empty() && had_data && decrypted == "{}";
    (env, error)
}

/// Build the whole archive. The returned file is rewound and ready to stream.
pub async fn build_archive(
    state: &SharedState,
    scope_label: &str,
    exported_by: &str,
    services: Vec<ExportService>,
    password: String,
    include_dumps: bool,
) -> Result<(File, ExportSummary)> {
    let tmp_dir = state.config.data_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir)?;
    let file = tempfile::tempfile_in(&tmp_dir)?;
    let mut archive = Archive::new(file, password);

    let mut dirs = HashSet::new();
    let mut manifest_services = Vec::with_capacity(services.len());
    let mut dumps = 0usize;

    for svc in &services {
        let dir = unique_dir(&mut dirs, &svc.name, &svc.id);
        archive = archive
            .add_blocking(
                format!("services/{dir}/.env"),
                render_dotenv(&svc.env).into_bytes(),
                true,
            )
            .await?;
        if let Some(build_env) = &svc.build_env {
            archive = archive
                .add_blocking(
                    format!("services/{dir}/build.env"),
                    build_env.clone().into_bytes(),
                    true,
                )
                .await?;
        }

        let status = match dump_status(svc, include_dumps) {
            DumpStatus::Done { .. } => {
                let (a, files, errors) = dump_service(archive, svc, &dir).await?;
                archive = a;
                dumps += files.len();
                DumpStatus::Done { files, errors }
            }
            other => other,
        };
        manifest_services.push(manifest_entry(svc, &dir, status));
    }

    let manifest = serde_json::json!({
        "format": "pier-export/1",
        "pier_version": env!("CARGO_PKG_VERSION"),
        "created_at": chrono::Utc::now().to_rfc3339(),
        "exported_by": exported_by,
        "scope": scope_label,
        "services": manifest_services,
    });
    archive = archive
        .add_blocking(
            "manifest.json".into(),
            serde_json::to_vec_pretty(&manifest)?,
            true,
        )
        .await?;

    let file = tokio::task::spawn_blocking(move || archive.finish()).await??;
    Ok((
        file,
        ExportSummary {
            services: services.len(),
            dumps,
        },
    ))
}

/// Decide up front whether a service gets dumped. `Done` here is a
/// placeholder meaning "go dump it"; the real file list comes later.
fn dump_status(svc: &ExportService, include_dumps: bool) -> DumpStatus {
    let catalog = svc.catalog_id.as_deref().unwrap_or("");
    if !executor::supports_backup(catalog) {
        return DumpStatus::None;
    }
    if !include_dumps {
        return DumpStatus::Skipped {
            reason: "disabled".into(),
        };
    }
    if !svc.is_local {
        return DumpStatus::Skipped {
            reason: "remote server".into(),
        };
    }
    DumpStatus::Done {
        files: vec![],
        errors: vec![],
    }
}

/// Dump every logical DB of a local database service into
/// `databases/<dir>/`. A failing DB doesn't abort the export: its error goes
/// into the manifest and an `ERROR.txt` next to the dumps.
async fn dump_service(
    mut archive: Archive,
    svc: &ExportService,
    dir: &str,
) -> Result<(Archive, Vec<String>, Vec<String>)> {
    let catalog = svc.catalog_id.as_deref().unwrap_or("");
    let container = format!("pier-{}", svc.name);
    let env: HashMap<String, String> = svc.env.clone().into_iter().collect();
    let mut files = Vec::new();
    let mut errors = Vec::new();

    if catalog == "mongodb" {
        match executor::execute_cluster_backup(&container, catalog, &env, &svc.credentials).await {
            Ok(bytes) => {
                let path = format!("databases/{dir}/mongodb.archive.gz");
                archive = archive.add_blocking(path.clone(), bytes, false).await?;
                files.push(path);
            }
            Err(e) => errors.push(format!("mongodump: {e:#}")),
        }
    } else {
        let db_names: Vec<String> = if svc.credentials.is_empty() {
            default_db_name(catalog, &svc.env).into_iter().collect()
        } else {
            svc.credentials.iter().map(|c| c.db_name.clone()).collect()
        };
        if db_names.is_empty() {
            errors.push("no databases found".into());
        }
        let ext = match executor::per_db_dump_format(catalog) {
            Some(PerDbDumpFormat::PgCustom) => "dump",
            _ => "sql.gz",
        };
        for db_name in db_names {
            let cred = svc.credentials.iter().find(|c| c.db_name == db_name);
            match executor::execute_db_backup(&container, catalog, &env, cred, &db_name).await {
                Ok(bytes) => {
                    let path = format!("databases/{dir}/{}.{ext}", sanitize(&db_name));
                    archive = archive.add_blocking(path.clone(), bytes, false).await?;
                    files.push(path);
                }
                Err(e) => errors.push(format!("{db_name}: {e:#}")),
            }
        }
    }

    if !errors.is_empty() {
        archive = archive
            .add_blocking(
                format!("databases/{dir}/ERROR.txt"),
                errors.join("\n").into_bytes(),
                true,
            )
            .await?;
    }
    Ok((archive, files, errors))
}

/// Databases created by the image itself (no `database_credentials` row).
fn default_db_name(catalog: &str, env: &BTreeMap<String, String>) -> Option<String> {
    let pick = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| env.get(*k).filter(|v| !v.is_empty()).cloned())
    };
    match catalog {
        "postgresql" | "postgis" | "timescaledb" => {
            pick(&["POSTGRES_DB", "POSTGRES_USER"]).or_else(|| Some("postgres".into()))
        }
        "mysql" | "mariadb" => pick(&["MYSQL_DATABASE", "MARIADB_DATABASE"]),
        _ => None,
    }
}

fn manifest_entry(svc: &ExportService, dir: &str, dump: DumpStatus) -> serde_json::Value {
    serde_json::json!({
        "id": svc.id,
        "name": svc.name,
        "dir": dir,
        "project": svc.project,
        "server": {
            "id": svc.server_id,
            "name": svc.server_name,
            "local": svc.is_local,
        },
        "catalog_id": svc.catalog_id,
        "category": svc.category,
        "env": svc.env,
        "env_error": svc.env_error,
        "build_env": svc.build_env,
        "database_credentials": svc.credentials.iter().map(|c| serde_json::json!({
            "db_name": c.db_name,
            "username": c.username,
            "password": c.password,
        })).collect::<Vec<_>>(),
        "db_dump": dump,
    })
}

/// `.env` with only the container variables (UPPERCASE keys, as in the UI's
/// env editor); template vars live in the manifest. Values are single-quoted
/// (no interpolation) unless they contain a quote or newline.
pub fn render_dotenv(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (k, v) in env {
        if k.chars().any(|c| c.is_ascii_lowercase()) {
            continue;
        }
        let quoted = if v.contains('\'') || v.contains('\n') || v.contains('\r') {
            let escaped = v
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', "\\n")
                .replace('\r', "\\r");
            format!("\"{escaped}\"")
        } else {
            format!("'{v}'")
        };
        out.push_str(&format!("{k}={quoted}\n"));
    }
    out
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match s.trim_matches('.') {
        "" => "_".into(),
        t => t.to_string(),
    }
}

fn unique_dir(seen: &mut HashSet<String>, name: &str, id: &str) -> String {
    let base = sanitize(name);
    if seen.insert(base.clone()) {
        return base;
    }
    let alt = format!("{base}-{}", id.chars().take(8).collect::<String>());
    seen.insert(alt.clone());
    alt
}

/// Thin wrapper owning the zip writer and the archive password. Writes are
/// CPU-bound (deflate + AES), so callers hop to the blocking pool per entry.
pub struct Archive {
    zip: ZipWriter<File>,
    password: String,
}

impl Archive {
    pub fn new(file: File, password: String) -> Self {
        Self {
            zip: ZipWriter::new(file),
            password,
        }
    }

    /// `compress = false` for dumps that are already compressed.
    pub fn add(&mut self, path: &str, data: &[u8], compress: bool) -> Result<()> {
        let method = if compress {
            CompressionMethod::Deflated
        } else {
            CompressionMethod::Stored
        };
        let options = FileOptions::<()>::default()
            .compression_method(method)
            .large_file(data.len() as u64 >= u32::MAX as u64)
            .unix_permissions(0o600)
            .with_aes_encryption(AesMode::Aes256, &self.password);
        self.zip.start_file(path, options)?;
        self.zip.write_all(data)?;
        Ok(())
    }

    async fn add_blocking(mut self, path: String, data: Vec<u8>, compress: bool) -> Result<Self> {
        tokio::task::spawn_blocking(move || {
            self.add(&path, &data, compress)?;
            Ok(self)
        })
        .await?
    }

    pub fn finish(self) -> Result<File> {
        let mut file = self.zip.finish()?;
        file.flush()?;
        file.seek(SeekFrom::Start(0))?;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn dotenv_skips_template_vars_and_quotes() {
        let out = render_dotenv(&env(&[
            ("API_KEY", "abc$def"),
            ("version", "16"),
            ("MULTI", "a\nb\"c"),
            ("QUOTE", "it's"),
        ]));
        assert_eq!(
            out,
            "API_KEY='abc$def'\nMULTI=\"a\\nb\\\"c\"\nQUOTE=\"it's\"\n"
        );
    }

    #[test]
    fn decode_env_flags_unreadable_ciphertext() {
        let (env, err) = decode_env(Some("ENC:garbage:garbage"));
        assert!(env.is_empty());
        assert!(err);
        let (env, err) = decode_env(None);
        assert!(env.is_empty());
        assert!(!err);
        let (env, err) = decode_env(Some(r#"{"A":"1"}"#));
        assert_eq!(env.get("A").map(String::as_str), Some("1"));
        assert!(!err);
    }

    #[test]
    fn remote_databases_are_skipped() {
        let mut svc = ExportService {
            id: "id".into(),
            name: "pg".into(),
            project: None,
            server_id: Some("srv".into()),
            server_name: None,
            is_local: false,
            catalog_id: Some("postgresql".into()),
            category: None,
            env: BTreeMap::new(),
            env_error: false,
            build_env: None,
            credentials: vec![],
        };
        assert_eq!(
            dump_status(&svc, true),
            DumpStatus::Skipped {
                reason: "remote server".into()
            }
        );
        svc.is_local = true;
        assert_eq!(
            dump_status(&svc, false),
            DumpStatus::Skipped {
                reason: "disabled".into()
            }
        );
        assert!(matches!(dump_status(&svc, true), DumpStatus::Done { .. }));
        svc.catalog_id = Some("redis".into());
        assert_eq!(dump_status(&svc, true), DumpStatus::None);
    }

    #[test]
    fn default_db_falls_back_to_env() {
        assert_eq!(
            default_db_name("postgresql", &env(&[("POSTGRES_DB", "app")])),
            Some("app".into())
        );
        assert_eq!(
            default_db_name("postgresql", &env(&[])),
            Some("postgres".into())
        );
        assert_eq!(default_db_name("mysql", &env(&[])), None);
    }

    #[test]
    fn unique_dirs_do_not_collide() {
        let mut seen = HashSet::new();
        assert_eq!(unique_dir(&mut seen, "my app", "12345678abc"), "my_app");
        assert_eq!(
            unique_dir(&mut seen, "my/app", "abcdefgh123"),
            "my_app-abcdefgh"
        );
        assert_eq!(sanitize(".."), "_");
    }

    #[test]
    fn archive_is_encrypted_and_readable_with_password() {
        let mut archive = Archive::new(tempfile::tempfile().unwrap(), "s3cret-pass".into());
        archive.add("services/app/.env", b"A='1'\n", true).unwrap();
        archive
            .add("databases/pg/app.dump", b"PGDMP", false)
            .unwrap();
        let file = archive.finish().unwrap();

        let mut zip = zip::ZipArchive::new(file).unwrap();
        assert_eq!(zip.len(), 2);
        assert!(zip.by_name_decrypt("services/app/.env", b"wrong").is_err());

        let mut buf = String::new();
        zip.by_name_decrypt("services/app/.env", b"s3cret-pass")
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, "A='1'\n");
    }
}
