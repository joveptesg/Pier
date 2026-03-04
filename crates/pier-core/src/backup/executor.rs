use std::collections::HashMap;

use anyhow::Result;
use tokio::process::Command;

/// Determine the dump command based on catalog_id.
pub fn dump_command(catalog_id: &str, env_vars: &HashMap<String, String>) -> Option<Vec<String>> {
    match catalog_id {
        "postgresql" => {
            let user = env_vars
                .get("POSTGRES_USER")
                .map(|s| s.as_str())
                .unwrap_or("postgres");
            let db = env_vars
                .get("POSTGRES_DB")
                .map(|s| s.as_str())
                .unwrap_or("postgres");
            Some(vec![
                "pg_dump".to_string(),
                "-U".to_string(),
                user.to_string(),
                "-d".to_string(),
                db.to_string(),
            ])
        }
        "mysql" | "mariadb" => {
            let pass_key = if catalog_id == "mariadb" {
                "MARIADB_ROOT_PASSWORD"
            } else {
                "MYSQL_ROOT_PASSWORD"
            };
            let db_key = if catalog_id == "mariadb" {
                "MARIADB_DATABASE"
            } else {
                "MYSQL_DATABASE"
            };
            let password = env_vars.get(pass_key).map(|s| s.as_str()).unwrap_or("");
            let db = env_vars.get(db_key).map(|s| s.as_str()).unwrap_or("");
            Some(vec![
                "mysqldump".to_string(),
                "-uroot".to_string(),
                format!("-p{password}"),
                db.to_string(),
            ])
        }
        "mongodb" => {
            let user = env_vars
                .get("MONGO_INITDB_ROOT_USERNAME")
                .map(|s| s.as_str())
                .unwrap_or("root");
            let pass = env_vars
                .get("MONGO_INITDB_ROOT_PASSWORD")
                .map(|s| s.as_str())
                .unwrap_or("");
            Some(vec![
                "mongodump".to_string(),
                "--archive".to_string(),
                format!("--username={user}"),
                format!("--password={pass}"),
                "--authenticationDatabase=admin".to_string(),
            ])
        }
        "redis" => Some(vec!["redis-cli".to_string(), "BGSAVE".to_string()]),
        "clickhouse" => Some(vec![
            "clickhouse-client".to_string(),
            "--query".to_string(),
            "SELECT * FROM system.tables FORMAT Native".to_string(),
        ]),
        _ => None,
    }
}

/// Execute a backup by running a dump command inside the container.
/// Returns the raw backup bytes.
pub async fn execute_backup(
    container_name: &str,
    catalog_id: &str,
    env_vars: &HashMap<String, String>,
) -> Result<Vec<u8>> {
    let cmd = dump_command(catalog_id, env_vars)
        .ok_or_else(|| anyhow::anyhow!("Unsupported backup type: {catalog_id}"))?;

    let mut args = vec!["exec".to_string(), container_name.to_string()];
    args.extend(cmd);

    let output = Command::new("docker").args(&args).output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("Backup command failed: {stderr}");
    }

    Ok(output.stdout)
}
