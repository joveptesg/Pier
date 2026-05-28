use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::process::Stdio;

use anyhow::Result;
use flate2::read::GzDecoder;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::executor::{supports_per_db_backup, DbCredential};

/// Whether the blob at this S3 key was stored with a Rust-side gzip wrapper.
/// Read here to decide if restore needs an explicit decompression step first.
///
/// Note: `pg_dump -Fc` blobs (`.dump`) carry their own zlib compression
/// inside the custom format, but `pg_restore` handles that transparently —
/// they do NOT need to be gunzipped first, and this returns `false` for them.
pub fn is_gzipped(s3_key: &str) -> bool {
    s3_key.ends_with(".gz")
}

/// Whether the blob at this S3 key is a PostgreSQL custom-format dump
/// (`pg_dump -Fc` output, written with a `.dump` suffix). These need to be
/// restored via `pg_restore`, not piped into `psql`.
///
/// Legacy PostgreSQL backups still use the `.sql.gz` suffix and remain
/// restorable through the plain-SQL path; this function returns `false` for
/// them, so the legacy `psql`-pipe code still kicks in.
pub fn is_pg_custom_format(s3_key: &str) -> bool {
    s3_key.ends_with(".dump")
}

/// Gunzip a byte slice. Returned bytes are whatever was wrapped — plain SQL
/// for per-DB SQL backups, a tar archive for cluster-wide SQL backups.
pub fn gunzip_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = GzDecoder::new(input);
    let mut out = Vec::with_capacity(input.len() * 4);
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

/// Extract a single per-database dump from a cluster-wide tar archive.
/// Returns `(is_pg_custom, bytes)`:
///  - `is_pg_custom = true` when the entry was named `<db>.dump` (PostgreSQL
///    custom format, restored via `pg_restore`),
///  - `is_pg_custom = false` when the entry was named `<db>.sql` (plain SQL,
///    used by MySQL/MariaDB and by legacy pre-migration PostgreSQL backups,
///    restored by piping into the engine CLI).
///
/// Archives are produced by `execute_cluster_backup`. Both entry-name
/// conventions are accepted so old `.sql`-only cluster tars keep working.
pub fn extract_db_from_tar(tar_bytes: &[u8], db_name: &str) -> Result<(bool, Vec<u8>)> {
    let mut archive = tar::Archive::new(Cursor::new(tar_bytes));
    let target_dump = format!("{db_name}.dump");
    let target_sql = format!("{db_name}.sql");
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();
        if path == target_dump || path == target_sql {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            return Ok((path == target_dump, buf));
        }
    }
    anyhow::bail!(
        "database '{db_name}' not found in cluster backup; archive entries do not match '{db_name}.dump' or '{db_name}.sql'"
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
/// - `is_pg_custom`: `true` when the dump bytes are PostgreSQL custom format
///   (`pg_dump -Fc`), restored via `pg_restore`. `false` for plain SQL
///   (legacy PostgreSQL `.sql.gz`, all MySQL/MariaDB), restored via
///   `psql`/`mysql` stdin pipe. Ignored for MySQL/MariaDB (always plain SQL).
/// - `dump_bytes`: the dump payload (plain SQL or pg-custom binary)
pub async fn execute_restore(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    is_pg_custom: bool,
    dump_bytes: Vec<u8>,
) -> Result<()> {
    if !supports_per_db_backup(catalog_id) {
        anyhow::bail!("per-DB restore not supported for {catalog_id}");
    }

    match catalog_id {
        "postgresql" | "postgis" | "timescaledb" => {
            if is_pg_custom {
                restore_postgres_custom(container_name, env_vars, target_db, owner, dump_bytes)
                    .await
            } else {
                restore_postgres(container_name, env_vars, target_db, owner, dump_bytes).await
            }
        }
        "mysql" | "mariadb" => {
            restore_mysql(
                container_name,
                catalog_id,
                env_vars,
                target_db,
                owner,
                dump_bytes,
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

/// Terminate active sessions on the target DB, ensure the owner role exists
/// with the password Pier currently has on file, and drop+recreate the DB
/// under that owner. Shared by both Postgres restore paths (plain SQL via
/// psql and custom format via pg_restore).
///
/// Why role-sync: restore may target a fresh PostgreSQL cluster on another
/// VPS where the per-DB owner role has never been created. We use the
/// password from `database_credentials` (Pier's SQLite, the source of truth)
/// as a CREATE ROLE / ALTER ROLE — so on cross-cluster restore the role
/// appears, and on same-cluster restore the password is re-synced to whatever
/// is currently in Pier's UI (preventing drift from manual `\password` edits
/// inside the cluster).
async fn drop_and_recreate_pg_db(
    container_name: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
) -> Result<()> {
    let (root_user, root_pass) = pg_root_creds(env_vars);

    let role_sync = build_role_sync_sql(&owner.username, &owner.password)?;

    // pg_terminate_backend ignores our own session (the psql we're running in).
    let recreate_sql = format!(
        "{role_sync}\n\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
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
    .await
}

/// Tag used to dollar-quote the body of the role-sync DO block. Picked to be
/// extremely unlikely to appear in a user-chosen password, but we still bail
/// defensively if it does (see `build_role_sync_sql`).
const ROLE_SYNC_DOLLAR_TAG: &str = "pier_role_sync";

/// Tag used to dollar-quote the body of the post-restore ownership-fix DO
/// block. Distinct from `ROLE_SYNC_DOLLAR_TAG` so log/error output makes it
/// obvious which step ran.
const OWNER_FIX_DOLLAR_TAG: &str = "pier_owner_fix";

/// Build the DO block that creates the owner role if missing or resets its
/// password if it already exists. Both branches set LOGIN privilege.
///
/// The password is interpolated as a SQL string literal (single quotes
/// doubled). The whole DO body is wrapped in a `$pier_role_sync$` dollar-quote
/// tag so PL/pgSQL doesn't have to interpret the inner string literals.
/// We refuse passwords that contain the dollar tag itself — that would
/// terminate the dollar-quote prematurely. In practice this never matches
/// real passwords; the check is purely a defense-in-depth.
fn build_role_sync_sql(username: &str, password: &str) -> Result<String> {
    let dollar_tag = format!("${ROLE_SYNC_DOLLAR_TAG}$");
    if password.contains(&dollar_tag) {
        anyhow::bail!(
            "owner password contains the reserved sequence '{dollar_tag}' — \
             refusing to build role-sync SQL. Change the password in the \
             Databases UI before restoring."
        );
    }
    let user_ident = escape_pg_ident(username);
    let user_lit = escape_pg_str_lit(username);
    let pass_lit = escape_pg_str_lit(password);
    Ok(format!(
        "DO {tag}\n\
         BEGIN\n\
         IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{user_lit}') THEN\n\
             CREATE ROLE \"{user_ident}\" WITH LOGIN PASSWORD '{pass_lit}';\n\
         ELSE\n\
             ALTER ROLE \"{user_ident}\" WITH LOGIN PASSWORD '{pass_lit}';\n\
         END IF;\n\
         END\n\
         {tag};",
        tag = dollar_tag,
    ))
}

/// Escape a Postgres SQL string literal: double any embedded single quotes.
/// Caller still wraps the result in single quotes.
fn escape_pg_str_lit(s: &str) -> String {
    s.replace('\'', "''")
}

/// Build the post-restore ownership-fix SQL.
///
/// Context: `pg_restore` (and the legacy `psql` plain-SQL path) is run as the
/// cluster superuser so it can recreate any objects in the dump regardless of
/// the original owner. The side effect is that every restored object —
/// schemas, tables, sequences, views, functions, types — ends up owned by the
/// superuser, NOT by the per-DB owner. `CREATE DATABASE ... OWNER x` sets the
/// DB-level owner, but ownership does not cascade to the objects inside.
///
/// Without this fix the application role connects to "its own" database and
/// cannot see any tables — symptom is "DB is alive but `<owner>` sees no
/// tables / no permissions" after restore.
///
/// The DO block reassigns ownership of every object whose current owner is
/// `current_user` (the superuser running this block, post-restore) over to
/// the target role, across all user schemas. We exclude only the genuine
/// system catalog schemas (`pg_catalog`, `information_schema`, `pg_toast`,
/// and the per-session `pg_temp_*` / `pg_toast_temp_*`); PostGIS / TimescaleDB
/// extension schemas (`tiger`, `topology`, `postgis`, …) are intentionally
/// included — they are ordinary user schemas created via `CREATE EXTENSION`
/// and should belong to the DB owner for the same reason as `public`.
///
/// The role name is interpolated once as a SQL string literal into the DO
/// block's local variable, and each inner `EXECUTE format(... %I ...)` uses
/// Postgres' own identifier quoting — so identifiers with quotes/spaces are
/// handled correctly without us building each ALTER statement on the Rust
/// side. The DO body is dollar-quoted with `$pier_owner_fix$`; we refuse
/// owner names containing that exact tag (defense in depth — usernames are
/// validated upstream to `[A-Za-z0-9_]`, but the check costs nothing).
fn build_owner_reassignment_sql(owner: &str) -> Result<String> {
    let dollar_tag = format!("${OWNER_FIX_DOLLAR_TAG}$");
    if owner.contains(&dollar_tag) {
        anyhow::bail!(
            "owner role name contains the reserved sequence '{dollar_tag}' — \
             refusing to build ownership-fix SQL."
        );
    }
    let owner_lit = escape_pg_str_lit(owner);
    Ok(format!(
        "DO {tag}\n\
         DECLARE\n\
             target_role text := '{owner_lit}';\n\
             rec record;\n\
         BEGIN\n\
             FOR rec IN\n\
                 SELECT n.nspname\n\
                 FROM pg_namespace n\n\
                 JOIN pg_roles r ON n.nspowner = r.oid\n\
                 WHERE r.rolname = current_user\n\
                   AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')\n\
                   AND n.nspname NOT LIKE 'pg_temp_%'\n\
                   AND n.nspname NOT LIKE 'pg_toast_temp_%'\n\
             LOOP\n\
                 EXECUTE format('ALTER SCHEMA %I OWNER TO %I', rec.nspname, target_role);\n\
             END LOOP;\n\
             FOR rec IN\n\
                 SELECT n.nspname, c.relname\n\
                 FROM pg_class c\n\
                 JOIN pg_namespace n ON c.relnamespace = n.oid\n\
                 JOIN pg_roles r ON c.relowner = r.oid\n\
                 WHERE r.rolname = current_user\n\
                   AND c.relkind IN ('r','v','m','S','f','p')\n\
                   AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')\n\
                   AND n.nspname NOT LIKE 'pg_temp_%'\n\
                   AND n.nspname NOT LIKE 'pg_toast_temp_%'\n\
             LOOP\n\
                 EXECUTE format('ALTER TABLE %I.%I OWNER TO %I', rec.nspname, rec.relname, target_role);\n\
             END LOOP;\n\
             FOR rec IN\n\
                 SELECT n.nspname, p.proname, pg_get_function_identity_arguments(p.oid) AS args\n\
                 FROM pg_proc p\n\
                 JOIN pg_namespace n ON p.pronamespace = n.oid\n\
                 JOIN pg_roles r ON p.proowner = r.oid\n\
                 WHERE r.rolname = current_user\n\
                   AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')\n\
                   AND n.nspname NOT LIKE 'pg_temp_%'\n\
                   AND n.nspname NOT LIKE 'pg_toast_temp_%'\n\
             LOOP\n\
                 EXECUTE format('ALTER FUNCTION %I.%I(%s) OWNER TO %I', rec.nspname, rec.proname, rec.args, target_role);\n\
             END LOOP;\n\
             FOR rec IN\n\
                 SELECT n.nspname, t.typname\n\
                 FROM pg_type t\n\
                 JOIN pg_namespace n ON t.typnamespace = n.oid\n\
                 JOIN pg_roles r ON t.typowner = r.oid\n\
                 WHERE r.rolname = current_user\n\
                   AND t.typtype IN ('c','e','d')\n\
                   AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')\n\
                   AND n.nspname NOT LIKE 'pg_temp_%'\n\
                   AND n.nspname NOT LIKE 'pg_toast_temp_%'\n\
                   AND NOT EXISTS (SELECT 1 FROM pg_class c WHERE c.reltype = t.oid)\n\
             LOOP\n\
                 EXECUTE format('ALTER TYPE %I.%I OWNER TO %I', rec.nspname, rec.typname, target_role);\n\
             END LOOP;\n\
         END\n\
         {tag};",
        tag = dollar_tag,
    ))
}

/// Restore from a plain-SQL dump (legacy `.sql.gz` PostgreSQL backups). Drops
/// and recreates the target DB, then streams the SQL into `psql` as the owner.
async fn restore_postgres(
    container_name: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    sql_bytes: Vec<u8>,
) -> Result<()> {
    drop_and_recreate_pg_db(container_name, env_vars, target_db, owner).await?;
    // Stream the SQL as the cluster superuser — symmetric with how it was
    // dumped (always under POSTGRES_USER) and bypasses any per-schema ACL
    // surprises (PostGIS `tiger`/`topology` objects owned by `postgres`).
    let (root_user, root_pass) = pg_root_creds(env_vars);
    run_psql(
        container_name,
        &root_user,
        &root_pass,
        Some(target_db),
        sql_bytes,
    )
    .await?;
    fix_object_ownership(container_name, &root_user, &root_pass, target_db, owner).await?;
    Ok(())
}

/// Restore from a `pg_dump -Fc` custom-format dump. Drops and recreates the
/// target DB, then streams the binary dump into `pg_restore` as the cluster
/// superuser.
///
/// Why superuser: dumps are produced under `POSTGRES_USER` (so the dump can
/// include PostGIS reference schemas owned by `postgres`); restoring under
/// the per-DB owner would re-trigger the same permission errors when
/// `pg_restore` tries to recreate those objects. `--no-owner --no-privileges`
/// strip ownership/grants from the dump so the result is owned by the
/// `drop_and_recreate_pg_db`-set owner, not by `postgres`.
///
/// Streaming via stdin precludes parallel restore (`pg_restore -j` requires
/// random-access on an on-disk file), but custom format still wins over plain
/// SQL: data loads via binary COPY, indexes and FK are built only after data
/// is in, and selective restore / better error reporting come for free.
async fn restore_postgres_custom(
    container_name: &str,
    env_vars: &HashMap<String, String>,
    target_db: &str,
    owner: &DbCredential,
    dump_bytes: Vec<u8>,
) -> Result<()> {
    drop_and_recreate_pg_db(container_name, env_vars, target_db, owner).await?;

    let (root_user, root_pass) = pg_root_creds(env_vars);
    let args = vec![
        "exec".to_string(),
        "-i".to_string(),
        container_name.to_string(),
        "env".to_string(),
        format!("PGPASSWORD={root_pass}"),
        "pg_restore".to_string(),
        "-U".to_string(),
        root_user.clone(),
        "-d".to_string(),
        target_db.to_string(),
        "--no-owner".to_string(),
        "--no-privileges".to_string(),
        "--exit-on-error".to_string(),
    ];
    pipe_to_docker(&args, dump_bytes).await?;
    fix_object_ownership(container_name, &root_user, &root_pass, target_db, owner).await
}

/// Connect to the freshly-restored target DB as the cluster superuser and run
/// the ownership-fix DO block (see [`build_owner_reassignment_sql`]). Must be
/// called AFTER the dump has been loaded — at that point all restored objects
/// are owned by the superuser, and this step transfers them to the per-DB
/// owner role that owns the database itself.
async fn fix_object_ownership(
    container_name: &str,
    root_user: &str,
    root_pass: &str,
    target_db: &str,
    owner: &DbCredential,
) -> Result<()> {
    let fix_sql = build_owner_reassignment_sql(&owner.username)?;
    run_psql(
        container_name,
        root_user,
        root_pass,
        Some(target_db),
        fix_sql.into_bytes(),
    )
    .await
}

/// Resolve the cluster superuser credentials from the service env. Default
/// user is `postgres` (matches the bitnami/postgres image default).
fn pg_root_creds(env_vars: &HashMap<String, String>) -> (String, String) {
    let user = env_vars
        .get("POSTGRES_USER")
        .cloned()
        .unwrap_or_else(|| "postgres".into());
    let pass = env_vars
        .get("POSTGRES_PASSWORD")
        .cloned()
        .unwrap_or_default();
    (user, pass)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::<u8>::new());
        for (name, body) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *body).unwrap();
        }
        builder.into_inner().unwrap()
    }

    #[test]
    fn pg_custom_format_detected_by_suffix() {
        assert!(is_pg_custom_format("svc/db_app_20260504.dump"));
        assert!(!is_pg_custom_format("svc/db_app_20260504.sql.gz"));
        assert!(!is_pg_custom_format("svc/_cluster_20260504.tar.gz"));
        assert!(!is_pg_custom_format("svc/db_app_20260504.archive.gz"));
    }

    #[test]
    fn extract_db_from_tar_finds_dump_entry() {
        let tar = build_tar(&[("appdb.dump", b"PGDMPbinary"), ("other.sql", b"-- other")]);
        let (is_pg_custom, bytes) = extract_db_from_tar(&tar, "appdb").unwrap();
        assert!(is_pg_custom, "appdb.dump should map to is_pg_custom=true");
        assert_eq!(bytes, b"PGDMPbinary");
    }

    #[test]
    fn extract_db_from_tar_finds_legacy_sql_entry() {
        let tar = build_tar(&[("legacy.sql", b"-- legacy SQL")]);
        let (is_pg_custom, bytes) = extract_db_from_tar(&tar, "legacy").unwrap();
        assert!(
            !is_pg_custom,
            "legacy.sql should map to is_pg_custom=false (plain SQL path)"
        );
        assert_eq!(bytes, b"-- legacy SQL");
    }

    #[test]
    fn extract_db_from_tar_errors_when_missing() {
        let tar = build_tar(&[("a.sql", b"data")]);
        let err = extract_db_from_tar(&tar, "missing").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing"), "error should name the DB: {msg}");
    }

    #[test]
    fn is_gzipped_unaffected_by_dump_suffix() {
        assert!(is_gzipped("svc/x.sql.gz"));
        assert!(is_gzipped("svc/x.tar.gz"));
        assert!(!is_gzipped("svc/x.dump"));
    }

    #[test]
    fn role_sync_sql_creates_or_alters_role() {
        let sql = build_role_sync_sql("appuser", "s3cret").unwrap();
        // Both branches must be emitted — this is what makes restore work
        // both on a fresh cluster (CREATE ROLE branch) and on the same
        // cluster (ALTER ROLE branch — re-sync the password).
        assert!(sql.contains("CREATE ROLE \"appuser\" WITH LOGIN PASSWORD 's3cret'"));
        assert!(sql.contains("ALTER ROLE \"appuser\" WITH LOGIN PASSWORD 's3cret'"));
        assert!(sql.contains("SELECT 1 FROM pg_roles WHERE rolname = 'appuser'"));
        // Dollar-quoted DO body so inner string literals don't need escaping
        // beyond the single-quote double.
        assert!(sql.starts_with("DO $pier_role_sync$"));
        assert!(sql.trim_end().ends_with("$pier_role_sync$;"));
    }

    #[test]
    fn role_sync_sql_escapes_single_quote_in_password() {
        let sql = build_role_sync_sql("u", "pa'ss").unwrap();
        // Single quote in password must be doubled to be a valid SQL string
        // literal — otherwise psql breaks out of the string and parses the
        // tail as SQL.
        assert!(sql.contains("PASSWORD 'pa''ss'"));
        assert!(!sql.contains("PASSWORD 'pa'ss'"));
    }

    #[test]
    fn role_sync_sql_escapes_double_quote_in_username() {
        let sql = build_role_sync_sql("we\"ird", "p").unwrap();
        // Double quote inside an identifier must be doubled.
        assert!(sql.contains("CREATE ROLE \"we\"\"ird\""));
        assert!(sql.contains("ALTER ROLE \"we\"\"ird\""));
        // Username inside a string literal does NOT get its double-quotes
        // doubled — only single quotes need escaping there.
        assert!(sql.contains("rolname = 'we\"ird'"));
    }

    #[test]
    fn role_sync_sql_rejects_password_containing_dollar_tag() {
        // Defense-in-depth: a password containing the dollar tag would
        // terminate the dollar-quote prematurely. We refuse to build the SQL.
        let err = build_role_sync_sql("u", "abc$pier_role_sync$xyz").unwrap_err();
        assert!(
            err.to_string().contains("$pier_role_sync$"),
            "error should mention the reserved sequence: {err}"
        );
    }

    #[test]
    fn owner_reassignment_sql_handles_schemas_relations_funcs_types() {
        let sql = build_owner_reassignment_sql("flowfinadm").unwrap();
        // All four ALTER forms must be present — each fixes a different class
        // of object that the superuser-driven restore left owned by postgres.
        assert!(sql.contains("ALTER SCHEMA %I OWNER TO %I"));
        assert!(sql.contains("ALTER TABLE %I.%I OWNER TO %I"));
        assert!(sql.contains("ALTER FUNCTION %I.%I(%s) OWNER TO %I"));
        assert!(sql.contains("ALTER TYPE %I.%I OWNER TO %I"));
        // Wrapped in the dedicated dollar-quoted DO block.
        assert!(sql.starts_with("DO $pier_owner_fix$"));
        assert!(sql.trim_end().ends_with("$pier_owner_fix$;"));
        // Role name interpolated once into the local variable.
        assert!(sql.contains("target_role text := 'flowfinadm';"));
        // Filter on current_user — i.e. the role that just ran pg_restore.
        assert!(sql.contains("WHERE r.rolname = current_user"));
        // Relkind covers tables, views, matviews, sequences, foreign tables,
        // partitioned tables. Drop any of these and restore breaks silently
        // for that object kind.
        assert!(sql.contains("c.relkind IN ('r','v','m','S','f','p')"));
        // Types filter: composite/enum/domain only, and skip implicit row
        // types whose ownership flows from their table.
        assert!(sql.contains("t.typtype IN ('c','e','d')"));
        assert!(sql.contains("NOT EXISTS (SELECT 1 FROM pg_class c WHERE c.reltype = t.oid)"));
    }

    #[test]
    fn owner_reassignment_sql_excludes_only_real_system_schemas() {
        let sql = build_owner_reassignment_sql("appuser").unwrap();
        // Genuine system catalog schemas — never reassign.
        assert!(sql.contains("'pg_catalog','information_schema','pg_toast'"));
        assert!(sql.contains("NOT LIKE 'pg_temp_%'"));
        assert!(sql.contains("NOT LIKE 'pg_toast_temp_%'"));
        // PostGIS / TimescaleDB extension schemas are ordinary user schemas
        // created via CREATE EXTENSION. They MUST be reassigned to the DB
        // owner so the app role can use the extension without GRANTs.
        // Belt-and-suspenders: assert these aren't smuggled into the exclude
        // list by a future "safety" edit.
        assert!(!sql.contains("'tiger'"));
        assert!(!sql.contains("'topology'"));
        assert!(!sql.contains("'postgis'"));
    }

    #[test]
    fn owner_reassignment_sql_escapes_single_quote_in_username() {
        // Usernames are validated to [A-Za-z0-9_] upstream, but the SQL
        // builder must still be string-literal-safe — otherwise a future
        // username relaxation silently turns into SQL injection.
        let sql = build_owner_reassignment_sql("o'brien").unwrap();
        assert!(sql.contains("target_role text := 'o''brien';"));
        assert!(!sql.contains("target_role text := 'o'brien';"));
    }

    #[test]
    fn owner_reassignment_sql_rejects_username_with_dollar_tag() {
        // Defense-in-depth, symmetric with build_role_sync_sql: a username
        // containing the dollar tag would terminate the dollar-quoted DO
        // block prematurely.
        let err = build_owner_reassignment_sql("abc$pier_owner_fix$xyz").unwrap_err();
        assert!(
            err.to_string().contains("$pier_owner_fix$"),
            "error should mention the reserved sequence: {err}"
        );
    }
}
