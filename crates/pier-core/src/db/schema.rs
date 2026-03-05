use anyhow::Result;
use rusqlite::Connection;

const MIGRATIONS: &[&str] = &[
    // Migration 1: Initial schema
    r#"
    CREATE TABLE IF NOT EXISTS users (
        id          TEXT PRIMARY KEY NOT NULL,
        username    TEXT NOT NULL UNIQUE,
        email       TEXT NOT NULL UNIQUE,
        password    TEXT NOT NULL,
        role        TEXT NOT NULL DEFAULT 'admin',
        is_active   INTEGER NOT NULL DEFAULT 1,
        created_at  TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS sessions (
        id          TEXT PRIMARY KEY NOT NULL,
        user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        ip_address  TEXT,
        user_agent  TEXT,
        expires_at  TEXT NOT NULL,
        created_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_sessions_user_id ON sessions(user_id);
    CREATE INDEX IF NOT EXISTS idx_sessions_expires_at ON sessions(expires_at);

    CREATE TABLE IF NOT EXISTS projects (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL UNIQUE,
        description     TEXT NOT NULL DEFAULT '',
        port_range_start INTEGER,
        port_range_end   INTEGER,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS services (
        id              TEXT PRIMARY KEY NOT NULL,
        project_id      TEXT REFERENCES projects(id) ON DELETE SET NULL,
        name            TEXT NOT NULL,
        service_type    TEXT NOT NULL DEFAULT 'container',
        container_id    TEXT,
        compose_path    TEXT,
        compose_content TEXT,
        status          TEXT NOT NULL DEFAULT 'unknown',
        port            INTEGER,
        domain          TEXT,
        image           TEXT,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_services_project_id ON services(project_id);

    CREATE TABLE IF NOT EXISTS deployment_logs (
        id          TEXT PRIMARY KEY NOT NULL,
        service_id  TEXT REFERENCES services(id) ON DELETE CASCADE,
        action      TEXT NOT NULL,
        status      TEXT NOT NULL DEFAULT 'pending',
        output      TEXT NOT NULL DEFAULT '',
        triggered_by TEXT,
        started_at  TEXT NOT NULL DEFAULT (datetime('now')),
        finished_at TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_deployment_logs_service_id ON deployment_logs(service_id);

    CREATE TABLE IF NOT EXISTS settings (
        key         TEXT PRIMARY KEY NOT NULL,
        value       TEXT NOT NULL,
        updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
    // Migration 2: Catalog resources + port allocations
    r#"
    ALTER TABLE services ADD COLUMN catalog_id TEXT;
    ALTER TABLE services ADD COLUMN category TEXT;
    ALTER TABLE services ADD COLUMN env_json TEXT;
    ALTER TABLE services ADD COLUMN volumes_json TEXT;

    CREATE TABLE IF NOT EXISTS port_allocations (
        id              TEXT PRIMARY KEY NOT NULL,
        service_id      TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        port_name       TEXT NOT NULL,
        host_port       INTEGER NOT NULL UNIQUE,
        container_port  INTEGER NOT NULL,
        protocol        TEXT NOT NULL DEFAULT 'tcp',
        created_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_port_alloc_service ON port_allocations(service_id);
    "#,
    // Migration 3: Git sources, S3 storages
    r#"
    CREATE TABLE IF NOT EXISTS git_sources (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL,
        source_type     TEXT NOT NULL DEFAULT 'github',
        base_url        TEXT NOT NULL,
        access_token    TEXT,
        is_active       INTEGER NOT NULL DEFAULT 1,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );

    CREATE TABLE IF NOT EXISTS source_repos (
        id              TEXT PRIMARY KEY NOT NULL,
        source_id       TEXT NOT NULL REFERENCES git_sources(id) ON DELETE CASCADE,
        project_id      TEXT REFERENCES projects(id) ON DELETE SET NULL,
        repo_name       TEXT NOT NULL,
        repo_url        TEXT NOT NULL,
        default_branch  TEXT NOT NULL DEFAULT 'main',
        is_private      INTEGER NOT NULL DEFAULT 0,
        created_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_source_repos_source ON source_repos(source_id);

    CREATE TABLE IF NOT EXISTS s3_storages (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL,
        storage_type    TEXT NOT NULL DEFAULT 's3',
        endpoint        TEXT NOT NULL,
        region          TEXT NOT NULL DEFAULT '',
        bucket          TEXT NOT NULL,
        access_key      TEXT NOT NULL,
        secret_key      TEXT NOT NULL,
        is_active       INTEGER NOT NULL DEFAULT 1,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
    // Migration 4: Backup scheduling
    r#"
    CREATE TABLE IF NOT EXISTS backup_schedules (
        id              TEXT PRIMARY KEY NOT NULL,
        service_id      TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        s3_storage_id   TEXT NOT NULL REFERENCES s3_storages(id) ON DELETE CASCADE,
        cron_expression TEXT NOT NULL DEFAULT '0 2 * * *',
        retention_count INTEGER NOT NULL DEFAULT 7,
        is_active       INTEGER NOT NULL DEFAULT 1,
        last_run_at     TEXT,
        next_run_at     TEXT,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_backup_sched_service ON backup_schedules(service_id);

    CREATE TABLE IF NOT EXISTS backups (
        id              TEXT PRIMARY KEY NOT NULL,
        schedule_id     TEXT REFERENCES backup_schedules(id) ON DELETE SET NULL,
        service_id      TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        s3_storage_id   TEXT NOT NULL REFERENCES s3_storages(id) ON DELETE CASCADE,
        s3_key          TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'pending',
        size_bytes      INTEGER NOT NULL DEFAULT 0,
        error_message   TEXT,
        triggered_by    TEXT NOT NULL DEFAULT 'schedule',
        started_at      TEXT NOT NULL DEFAULT (datetime('now')),
        finished_at     TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_backups_service ON backups(service_id);
    CREATE INDEX IF NOT EXISTS idx_backups_schedule ON backups(schedule_id);
    "#,
    // Migration 5: Servers (cluster mode)
    r#"
    CREATE TABLE IF NOT EXISTS servers (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL,
        host            TEXT NOT NULL,
        port            INTEGER NOT NULL DEFAULT 3001,
        agent_token     TEXT NOT NULL,
        ssh_user        TEXT,
        ssh_port        INTEGER DEFAULT 22,
        status          TEXT NOT NULL DEFAULT 'pending',
        last_heartbeat  TEXT,
        os_info         TEXT,
        cpu_count       INTEGER,
        memory_total    INTEGER,
        docker_version  TEXT,
        is_local        INTEGER NOT NULL DEFAULT 0,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );

    ALTER TABLE services ADD COLUMN server_id TEXT REFERENCES servers(id) ON DELETE SET NULL;
    ALTER TABLE services ADD COLUMN cluster_mode TEXT;
    ALTER TABLE services ADD COLUMN cluster_config_json TEXT;
    "#,
    // Migration 6: GitHub App columns for git_sources
    r#"
    ALTER TABLE git_sources ADD COLUMN app_id TEXT;
    ALTER TABLE git_sources ADD COLUMN installation_id INTEGER;
    ALTER TABLE git_sources ADD COLUMN private_key TEXT;
    "#,
    // Migration 7: Domains + proxy
    r#"
    CREATE TABLE IF NOT EXISTS domains (
        id              TEXT PRIMARY KEY NOT NULL,
        domain          TEXT NOT NULL UNIQUE,
        service_id      TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        ssl_status      TEXT NOT NULL DEFAULT 'pending',
        ssl_expires_at  TEXT,
        ssl_provider    TEXT NOT NULL DEFAULT 'letsencrypt',
        is_generated    INTEGER NOT NULL DEFAULT 0,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_domains_service ON domains(service_id);
    CREATE INDEX IF NOT EXISTS idx_domains_domain ON domains(domain);
    "#,
];

/// Run all pending database migrations.
pub fn run_migrations(conn: &Connection) -> Result<()> {
    // Create migrations tracking table
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS _migrations (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );",
    )?;

    let current_version: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM _migrations",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    for (i, migration) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as u32;
        if version > current_version {
            tracing::info!("Running migration {version}...");
            conn.execute_batch(migration)?;
            conn.execute(
                "INSERT INTO _migrations (version) VALUES (?1)",
                [version],
            )?;
        }
    }

    Ok(())
}
