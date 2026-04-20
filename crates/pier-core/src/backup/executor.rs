use std::collections::HashMap;
use std::io::Cursor;

use anyhow::Result;
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
/// Returns `None` for catalogs that don't support per-DB backup (MongoDB uses
/// a different code path — see `mongo_dump_args`).
fn per_db_dump_args(
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    cred: Option<&DbCredential>,
    db_name: &str,
) -> Option<Vec<String>> {
    match catalog_id {
        "postgresql" => {
            let (user, pass) = match cred {
                Some(c) => (c.username.clone(), c.password.clone()),
                None => (
                    env_vars
                        .get("POSTGRES_USER")
                        .cloned()
                        .unwrap_or_else(|| "postgres".into()),
                    env_vars.get("POSTGRES_PASSWORD").cloned().unwrap_or_default(),
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
            // Per-DB credential exists in the UI layer but we authenticate as
            // root for backups — simpler and avoids per-user role plumbing.
            let _ = cred;
            Some(vec![
                "mongodump".into(),
                "--archive".into(),
                format!("--db={db_name}"),
                format!("--username={user}"),
                format!("--password={pass}"),
                "--authenticationDatabase=admin".into(),
            ])
        }
        _ => None,
    }
}

/// Full-instance dump (used for MongoDB cluster-wide, where there's no
/// per-database tracking in PR 2 yet).
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
        format!("--username={user}"),
        format!("--password={pass}"),
        "--authenticationDatabase=admin".into(),
    ]
}

/// Returns true if the catalog supports the backup feature at all.
/// Mirrors the frontend `supportsBackup` gate.
pub fn supports_backup(catalog_id: &str) -> bool {
    matches!(catalog_id, "postgresql" | "mysql" | "mariadb" | "mongodb")
}

/// Returns true if the catalog supports per-database backup and restore.
pub fn supports_per_db_backup(catalog_id: &str) -> bool {
    matches!(catalog_id, "postgresql" | "mysql" | "mariadb" | "mongodb")
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

/// Execute a backup for a single logical database inside the container.
pub async fn execute_db_backup(
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

/// Execute a cluster-wide backup.
///
/// For Postgres/MySQL/MariaDB: iterates over `credentials`, runs per-DB dump
/// for each, and bundles the results into an uncompressed tar archive. The
/// archive contains one `<db_name>.sql` entry per database, so a single DB can
/// be extracted later for restore.
///
/// For MongoDB: runs a single `mongodump --archive` over the whole instance
/// (credentials list is ignored).
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

    let buf: Vec<u8> = Vec::new();
    let cursor = Cursor::new(buf);
    let mut builder = tar::Builder::new(cursor);

    for cred in credentials {
        let dump =
            execute_db_backup(container_name, catalog_id, env_vars, Some(cred), &cred.db_name)
                .await?;
        let mut header = tar::Header::new_gnu();
        header.set_size(dump.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        let entry_name = format!("{}.sql", cred.db_name);
        builder.append_data(&mut header, entry_name, dump.as_slice())?;
    }

    let cursor = builder.into_inner()?;
    Ok(cursor.into_inner())
}
