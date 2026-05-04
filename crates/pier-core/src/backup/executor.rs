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

/// Build the docker-exec argv for a per-database dump.
/// Returns `None` for catalogs that don't support per-DB backup.
/// Mongo args include `--gzip`, so mongodump writes a compressed archive
/// directly; SQL dumps come out plain and are gzipped in Rust afterwards.
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
            Some(vec![
                "env".into(),
                format!("PGPASSWORD={pass}"),
                "pg_dump".into(),
                "-U".into(),
                user,
                "-d".into(),
                db_name.to_string(),
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
/// Returns bytes ready to store in S3: gzipped for Postgres/MySQL/MariaDB
/// (via Rust flate2), already compressed for Mongo (via mongodump `--gzip`).
pub async fn execute_db_backup(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    cred: Option<&DbCredential>,
    db_name: &str,
) -> Result<Vec<u8>> {
    let raw = execute_db_dump_raw(container_name, catalog_id, env_vars, cred, db_name).await?;
    if catalog_id == "mongodb" {
        Ok(raw)
    } else {
        gzip_bytes(&raw)
    }
}

/// Execute a cluster-wide backup.
///
/// For Postgres/MySQL/MariaDB: iterates over `credentials`, dumps each
/// database plain, bundles them into a gzipped tar archive. Individual
/// entries (`<db_name>.sql`) are uncompressed so extraction can read them
/// directly; the whole tar is gzipped once for storage.
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
        let entry_name = format!("{}.sql", cred.db_name);
        builder.append_data(&mut header, entry_name, dump.as_slice())?;
    }

    let gz = builder.into_inner()?;
    Ok(gz.finish()?)
}
