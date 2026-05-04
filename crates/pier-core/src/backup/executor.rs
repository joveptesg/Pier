use std::collections::HashMap;
use std::io::Write;

use anyhow::Result;
use flate2::write::GzEncoder;
use flate2::Compression;
use tokio::process::Command;

/// Credentials for one logical database inside a DB instance.
/// Populated from the `database_credentials` table.
#[derive(Debug, Clone)]
pub struct DbCredential {
    pub db_name: String,
    pub username: String,
    pub password: String,
}

/// Wire format produced by a per-database dump for a given engine. Drives
/// both the S3 key extension (in `scheduler::build_s3_key`) and whether the
/// raw bytes need a Rust-side gzip wrapper before storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerDbDumpFormat {
    /// PostgreSQL custom format (`pg_dump -Fc`). Self-compressed binary,
    /// restored with `pg_restore`. Stored as `.dump`.
    PgCustom,
    /// Plain SQL, gzipped in Rust via `flate2`. Used by MySQL/MariaDB.
    /// Stored as `.sql.gz`, restored by piping into the engine CLI.
    SqlGzipped,
    /// `mongodump --archive --gzip` output. Stored as `.archive.gz`.
    MongoArchive,
}

/// Returns the dump format for a per-database backup of the given catalog,
/// or `None` if the catalog doesn't support per-DB backup at all.
pub fn per_db_dump_format(catalog_id: &str) -> Option<PerDbDumpFormat> {
    match catalog_id {
        "postgresql" | "postgis" => Some(PerDbDumpFormat::PgCustom),
        "mysql" | "mariadb" => Some(PerDbDumpFormat::SqlGzipped),
        "mongodb" => Some(PerDbDumpFormat::MongoArchive),
        _ => None,
    }
}

/// Build the docker-exec argv for a per-database dump.
/// Returns `None` for catalogs that don't support per-DB backup.
///
/// Output formats per engine (must stay in sync with `per_db_dump_format`):
/// - PostgreSQL: `pg_dump -Fc` writes the custom format (binary, self-
///   compressed) to stdout; consumed by `pg_restore` on restore.
/// - MySQL/MariaDB: `mysqldump` writes plain SQL; gzipped in Rust afterwards.
/// - MongoDB: `mongodump --archive --gzip` writes a compressed archive
///   directly.
fn per_db_dump_args(
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    cred: Option<&DbCredential>,
    db_name: &str,
) -> Option<Vec<String>> {
    match catalog_id {
        "postgresql" | "postgis" => {
            let (user, pass) = match cred {
                Some(c) => (c.username.clone(), c.password.clone()),
                None => (
                    env_vars
                        .get("POSTGRES_USER")
                        .cloned()
                        .unwrap_or_else(|| "postgres".into()),
                    env_vars
                        .get("POSTGRES_PASSWORD")
                        .cloned()
                        .unwrap_or_default(),
                ),
            };
            // -Fc: custom format. Restored with pg_restore, supports
            //   selective restore and (with on-disk file) parallel jobs.
            // -Z 6: explicit zlib level for reproducibility across pg_dump
            //   versions (default is also 6, but pinning makes intent clear).
            // --no-owner: dump does not emit ALTER OWNER statements; we
            //   restore as the owner credential, so ownership is inherent.
            Some(vec![
                "env".into(),
                format!("PGPASSWORD={pass}"),
                "pg_dump".into(),
                "-U".into(),
                user,
                "-d".into(),
                db_name.to_string(),
                "-Fc".into(),
                "-Z".into(),
                "6".into(),
                "--no-owner".into(),
            ])
        }
        "mysql" | "mariadb" => {
            let pass_key = if catalog_id == "mariadb" {
                "MARIADB_ROOT_PASSWORD"
            } else {
                "MYSQL_ROOT_PASSWORD"
            };
            let password = env_vars.get(pass_key).cloned().unwrap_or_default();
            Some(vec![
                "mysqldump".into(),
                "-uroot".into(),
                format!("-p{password}"),
                db_name.to_string(),
            ])
        }
        "mongodb" => {
            let user = env_vars
                .get("MONGO_INITDB_ROOT_USERNAME")
                .cloned()
                .unwrap_or_else(|| "root".into());
            let pass = env_vars
                .get("MONGO_INITDB_ROOT_PASSWORD")
                .cloned()
                .unwrap_or_default();
            let _ = cred;
            Some(vec![
                "mongodump".into(),
                "--archive".into(),
                "--gzip".into(),
                format!("--db={db_name}"),
                format!("--username={user}"),
                format!("--password={pass}"),
                "--authenticationDatabase=admin".into(),
            ])
        }
        _ => None,
    }
}

/// Full-instance dump (cluster-wide MongoDB). `--gzip` gives a compressed
/// archive directly from mongodump.
fn mongo_dump_args(env_vars: &HashMap<String, String>) -> Vec<String> {
    let user = env_vars
        .get("MONGO_INITDB_ROOT_USERNAME")
        .cloned()
        .unwrap_or_else(|| "root".into());
    let pass = env_vars
        .get("MONGO_INITDB_ROOT_PASSWORD")
        .cloned()
        .unwrap_or_default();
    vec![
        "mongodump".into(),
        "--archive".into(),
        "--gzip".into(),
        format!("--username={user}"),
        format!("--password={pass}"),
        "--authenticationDatabase=admin".into(),
    ]
}

/// Returns true if the catalog supports the backup feature at all.
/// Mirrors the frontend `supportsBackup` gate.
pub fn supports_backup(catalog_id: &str) -> bool {
    matches!(
        catalog_id,
        "postgresql" | "postgis" | "mysql" | "mariadb" | "mongodb"
    )
}

/// Returns true if the catalog supports per-database backup and restore.
pub fn supports_per_db_backup(catalog_id: &str) -> bool {
    matches!(
        catalog_id,
        "postgresql" | "postgis" | "mysql" | "mariadb" | "mongodb"
    )
}

async fn docker_exec(container: &str, argv: &[String]) -> Result<Vec<u8>> {
    let mut args = vec!["exec".to_string(), container.to_string()];
    args.extend(argv.iter().cloned());
    let output = Command::new("docker").args(&args).output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("docker exec failed: {stderr}");
    }
    Ok(output.stdout)
}

fn gzip_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(input)?;
    Ok(enc.finish()?)
}

/// Raw per-database dump (no Rust-side compression). Used both as the
/// building block for `execute_db_backup` and for assembling cluster tar
/// archives, where entries stay uncompressed because the whole tar is
/// wrapped in a single GzEncoder.
async fn execute_db_dump_raw(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    cred: Option<&DbCredential>,
    db_name: &str,
) -> Result<Vec<u8>> {
    let argv = per_db_dump_args(catalog_id, env_vars, cred, db_name)
        .ok_or_else(|| anyhow::anyhow!("per-DB backup not supported for {catalog_id}"))?;
    docker_exec(container_name, &argv).await
}

/// Execute a backup for a single logical database inside the container.
/// Returns bytes ready to store in S3:
/// - Postgres: raw `pg_dump -Fc` output (already binary-compressed).
/// - MySQL/MariaDB: plain SQL gzipped in Rust via `flate2`.
/// - MongoDB: raw mongodump archive (already gzipped via `--gzip`).
pub async fn execute_db_backup(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    cred: Option<&DbCredential>,
    db_name: &str,
) -> Result<Vec<u8>> {
    let raw = execute_db_dump_raw(container_name, catalog_id, env_vars, cred, db_name).await?;
    match per_db_dump_format(catalog_id) {
        Some(PerDbDumpFormat::PgCustom) | Some(PerDbDumpFormat::MongoArchive) => Ok(raw),
        Some(PerDbDumpFormat::SqlGzipped) => gzip_bytes(&raw),
        None => anyhow::bail!("per-DB backup not supported for {catalog_id}"),
    }
}

/// Execute a cluster-wide backup.
///
/// For Postgres/MySQL/MariaDB: iterates over `credentials`, dumps each
/// database, bundles them into a gzipped tar archive. Entry names depend
/// on the engine:
/// - Postgres: `<db_name>.dump` (custom format, self-compressed). The outer
///   tar.gz adds little extra compression but is kept for S3-key uniformity.
/// - MySQL/MariaDB: `<db_name>.sql` (plain SQL, compressed by the outer gzip).
///
/// For MongoDB: runs a single `mongodump --gzip --archive` over the whole
/// instance (credentials list is ignored).
pub async fn execute_cluster_backup(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    credentials: &[DbCredential],
) -> Result<Vec<u8>> {
    if catalog_id == "mongodb" {
        return docker_exec(container_name, &mongo_dump_args(env_vars)).await;
    }

    if !supports_per_db_backup(catalog_id) {
        anyhow::bail!("cluster-wide backup not supported for {catalog_id}");
    }

    if credentials.is_empty() {
        anyhow::bail!(
            "no databases found for service — create at least one database in the Databases tab \
             before running a cluster-wide backup"
        );
    }

    let gz = GzEncoder::new(Vec::new(), Compression::default());
    let mut builder = tar::Builder::new(gz);

    for cred in credentials {
        let dump = execute_db_dump_raw(
            container_name,
            catalog_id,
            env_vars,
            Some(cred),
            &cred.db_name,
        )
        .await?;
        let mut header = tar::Header::new_gnu();
        header.set_size(dump.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let entry_ext = match per_db_dump_format(catalog_id) {
            Some(PerDbDumpFormat::PgCustom) => "dump",
            _ => "sql",
        };
        let entry_name = format!("{}.{entry_ext}", cred.db_name);
        builder.append_data(&mut header, entry_name, dump.as_slice())?;
    }

    let gz = builder.into_inner()?;
    Ok(gz.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_args_request_custom_format() {
        let env = HashMap::new();
        let cred = DbCredential {
            db_name: "appdb".into(),
            username: "owner".into(),
            password: "secret".into(),
        };
        let args = per_db_dump_args("postgresql", &env, Some(&cred), "appdb")
            .expect("postgresql per-DB args");
        assert!(args.iter().any(|a| a == "-Fc"), "expected -Fc in {args:?}");
        assert!(args.iter().any(|a| a == "--no-owner"));
        assert!(args.iter().any(|a| a == "pg_dump"));
        // PGPASSWORD must be passed via env, never on the pg_dump cmd line.
        assert!(args.iter().any(|a| a.starts_with("PGPASSWORD=")));
    }

    #[test]
    fn postgis_uses_pg_custom_format() {
        assert_eq!(
            per_db_dump_format("postgis"),
            Some(PerDbDumpFormat::PgCustom)
        );
    }

    #[test]
    fn mysql_stays_on_sql_gzip() {
        assert_eq!(
            per_db_dump_format("mysql"),
            Some(PerDbDumpFormat::SqlGzipped)
        );
        let mut env = HashMap::new();
        env.insert("MYSQL_ROOT_PASSWORD".into(), "rootpw".into());
        let args = per_db_dump_args("mysql", &env, None, "appdb").unwrap();
        // Must NOT have any -Fc / pg_dump leakage from a wrong match arm.
        assert!(!args.iter().any(|a| a == "-Fc"));
        assert!(args.iter().any(|a| a == "mysqldump"));
    }

    #[test]
    fn unsupported_catalog_has_no_format() {
        assert_eq!(per_db_dump_format("redis"), None);
        assert!(per_db_dump_args("redis", &HashMap::new(), None, "x").is_none());
    }
}
