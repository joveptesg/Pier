use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::process::Stdio;

use anyhow::Result;
use flate2::read::GzDecoder;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::executor::{supports_per_db_backup, DbCredential};

/// Whether the blob at this S3 key was stored with a gzip wrapper.
/// Written by `build_s3_key`; read here to decide if restore needs a
/// decompression step first.
pub fn is_gzipped(s3_key: &str) -> bool {
    s3_key.ends_with(".gz")
}

/// Gunzip a byte slice. Returned bytes are whatever was wrapped — plain SQL
/// for per-DB SQL backups, a tar archive for cluster-wide SQL backups.
pub fn gunzip_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(input);
    let mut out = Vec::with_capacity(input.len() * 4);
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

/// Extract a single per-database SQL file from a cluster-wide tar archive.
/// Returns the raw SQL bytes if found. Archives are produced by
/// `execute_cluster_backup` with entry names `<db_name>.sql`.
pub fn extract_db_from_tar(tar_bytes: &[u8], db_name: &str) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    let target = format!("{db_name}.sql");
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path == target {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok(buf);
        }
    }
    anyhow::bail!(
        "database '{db_name}' not found in cluster backup; archive entries do not match '{db_name}.sql'"
    )
}

/// Decide whether the given backup blob is a cluster-wide tar archive (needs
/// extraction) or an already-per-DB SQL dump (stream as-is). Cluster-wide
/// SQL backups are always tar-wrapped — we distinguish by the s3_key suffix,
/// matching both the pre-gzip `.tar` and the current `.tar.gz`.
pub fn is_cluster_archive(s3_key: &str) -> bool {
    s3_key.ends_with(".tar") || s3_key.ends_with(".tar.gz")
}

/// Restore a per-database backup into the target database inside the
/// container. Drops and recreates the target DB first, per the approved plan
/// ("Drop + recreate целевой БД").
///
/// Parameters:
/// - `container_name`: docker container of the DB service
/// - `catalog_id`: postgresql | mysql | mariadb (MongoDB is PR 4)
/// - `env_vars`: decrypted env for the service (for root creds)
/// - `target_db`: name of the DB to restore into (must already exist in
///   `database_credentials` so we know its owner)
/// - `owner`: credential row for `target_db`, used as the recreated DB's owner
///   (Postgres) or re-granted user (MySQL)
/// - `sql_bytes`: the plain SQL dump to pipe in
pub async fn execute_restore(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    sql_bytes: Vec<u8>,
) -> Result<()> {
    if !supports_per_db_backup(catalog_id) {
        anyhow::bail!("per-DB restore not supported for {catalog_id}");
    }

    match catalog_id {
        "postgresql" => {
            restore_postgres(container_name, env_vars, target_db, owner, sql_bytes).await
        }
        "mysql" | "mariadb" => {
            restore_mysql(
                container_name,
                catalog_id,
                env_vars,
                target_db,
                owner,
                sql_bytes,
            )
            .await
        }
        // Mongo takes a different entry point (`execute_mongo_restore`) because
        // its backups are opaque BSON archives, not plain SQL, and no tar
        // extraction happens up-stack.
        _ => unreachable!(),
    }
}

/// Restore a MongoDB database from a `mongodump --archive` blob. Works for
/// both per-DB archives (dumped with `--db=X`) and full-instance archives
/// (cluster-wide `mongodump --archive`); in both cases `--nsInclude=X.*`
/// limits the restore to the target DB, and `--drop` recreates collections.
/// If `gzipped` is true the archive was produced with `mongodump --gzip`
/// and we pass `--gzip` to mongorestore so it decompresses on the fly.
pub async fn execute_mongo_restore(
    container_name: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    gzipped: bool,
    archive_bytes: Vec<u8>,
) -> Result<()> {
    let user = env_vars
        .get("MONGO_INITDB_ROOT_USERNAME")
        .cloned()
        .unwrap_or_else(|| "root".into());
    let pass = env_vars
        .get("MONGO_INITDB_ROOT_PASSWORD")
        .cloned()
        .unwrap_or_default();
    let mut args = vec![
        "exec".to_string(),
        "-i".to_string(),
        container_name.to_string(),
        "mongorestore".to_string(),
        "--archive".to_string(),
        format!("--username={user}"),
        format!("--password={pass}"),
        "--authenticationDatabase=admin".to_string(),
        "--drop".to_string(),
        format!("--nsInclude={target_db}.*"),
    ];
    if gzipped {
        args.push("--gzip".to_string());
    }
    pipe_to_docker(&args, archive_bytes).await
}

async fn restore_postgres(
    container_name: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    sql_bytes: Vec<u8>,
) -> Result<()> {
    let root_user = env_vars
        .get("POSTGRES_USER")
        .cloned()
        .unwrap_or_else(|| "postgres".into());
    let root_pass = env_vars
        .get("POSTGRES_PASSWORD")
        .cloned()
        .unwrap_or_default();

    // 1. Terminate active sessions on the target DB, then drop and recreate.
    // pg_terminate_backend ignores our own session (the psql we're running in).
    let recreate_sql = format!(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
           WHERE datname = '{db}' AND pid <> pg_backend_pid();\n\
         DROP DATABASE IF EXISTS \"{db}\";\n\
         CREATE DATABASE \"{db}\" OWNER \"{owner}\";\n",
        db = escape_pg_ident(target_db),
        owner = escape_pg_ident(&owner.username),
    );
    run_psql(
        container_name,
        &root_user,
        &root_pass,
        None,
        recreate_sql.into_bytes(),
    )
    .await?;

    // 2. Stream the dump into the freshly-created DB as the owner.
    run_psql(
        container_name,
        &owner.username,
        &owner.password,
        Some(target_db),
        sql_bytes,
    )
    .await?;

    Ok(())
}

async fn run_psql(
    container_name: &str,
    user: &str,
    password: &str,
    db: Option<&str>,
    stdin_bytes: Vec<u8>,
) -> Result<()> {
    let mut args = vec![
        "exec".to_string(),
        "-i".to_string(),
        container_name.to_string(),
        "env".to_string(),
        format!("PGPASSWORD={password}"),
        "psql".to_string(),
        "-v".to_string(),
        "ON_ERROR_STOP=1".to_string(),
        "-U".to_string(),
        user.to_string(),
    ];
    match db {
        Some(d) => {
            args.push("-d".to_string());
            args.push(d.to_string());
        }
        None => {
            args.push("-d".to_string());
            args.push("postgres".to_string());
        }
    }

    pipe_to_docker(&args, stdin_bytes).await
}

async fn restore_mysql(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    sql_bytes: Vec<u8>,
) -> Result<()> {
    let pass_key = if catalog_id == "mariadb" {
        "MARIADB_ROOT_PASSWORD"
    } else {
        "MYSQL_ROOT_PASSWORD"
    };
    let root_pass = env_vars.get(pass_key).cloned().unwrap_or_default();

    // 1. Drop + recreate the target DB, then re-grant the owner's privileges.
    // mysqldump by default does not include user/GRANT statements, so CREATE
    // DATABASE alone leaves the owner without access — we must re-grant.
    let recreate_sql = format!(
        "DROP DATABASE IF EXISTS `{db}`; \
         CREATE DATABASE `{db}`; \
         GRANT ALL PRIVILEGES ON `{db}`.* TO '{user}'@'%'; \
         FLUSH PRIVILEGES;",
        db = escape_mysql_ident(target_db),
        user = escape_mysql_str(&owner.username),
    );
    run_mysql(container_name, &root_pass, None, recreate_sql.into_bytes()).await?;

    // 2. Stream the dump into the recreated DB.
    run_mysql(container_name, &root_pass, Some(target_db), sql_bytes).await?;

    Ok(())
}

async fn run_mysql(
    container_name: &str,
    root_pass: &str,
    db: Option<&str>,
    stdin_bytes: Vec<u8>,
) -> Result<()> {
    // `-e` is for inline SQL; with stdin piping we use no `-e` and let mysql
    // read from stdin when we DON'T pass `-e`.
    let mut args = vec![
        "exec".to_string(),
        "-i".to_string(),
        container_name.to_string(),
        "mysql".to_string(),
        "-uroot".to_string(),
        format!("-p{root_pass}"),
    ];
    if let Some(d) = db {
        args.push(d.to_string());
    }

    pipe_to_docker(&args, stdin_bytes).await
}

async fn pipe_to_docker(args: &[String], stdin_bytes: Vec<u8>) -> Result<()> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&stdin_bytes).await?;
        stdin.shutdown().await?;
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("restore command failed: {stderr}");
    }
    Ok(())
}

/// Escape a Postgres identifier by doubling any embedded quotes.
/// Callers still wrap the result in double quotes.
fn escape_pg_ident(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// Escape a MySQL identifier by doubling any embedded backticks.
fn escape_mysql_ident(s: &str) -> String {
    s.replace('`', "``")
}

/// Escape a MySQL string literal (single-quoted).
fn escape_mysql_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}
