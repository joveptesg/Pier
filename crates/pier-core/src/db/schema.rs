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
    // Migration 8: Ensure deployment_logs table exists
    r#"
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
    "#,
    // Migration 9: Git webhooks + auto-deploy pipeline
    r#"
    ALTER TABLE services ADD COLUMN git_source_id TEXT;
    ALTER TABLE services ADD COLUMN git_repo_url TEXT;
    ALTER TABLE services ADD COLUMN git_branch TEXT DEFAULT 'main';
    ALTER TABLE services ADD COLUMN git_webhook_secret TEXT;
    ALTER TABLE services ADD COLUMN build_strategy TEXT DEFAULT 'dockerfile';
    ALTER TABLE services ADD COLUMN previous_image_tag TEXT;

    CREATE TABLE IF NOT EXISTS deployments (
        id              TEXT PRIMARY KEY NOT NULL,
        service_id      TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        commit_sha      TEXT,
        commit_message  TEXT,
        branch          TEXT,
        status          TEXT NOT NULL DEFAULT 'pending',
        build_log       TEXT NOT NULL DEFAULT '',
        image_tag       TEXT,
        triggered_by    TEXT NOT NULL DEFAULT 'webhook',
        duration_secs   INTEGER,
        started_at      TEXT NOT NULL DEFAULT (datetime('now')),
        finished_at     TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_deployments_service_id ON deployments(service_id);
    "#,
    // Migration 10: Port visibility (public/private toggle)
    r#"
    ALTER TABLE port_allocations ADD COLUMN is_public INTEGER NOT NULL DEFAULT 0;
    "#,
    // Migration 11: Docker networks management
    r#"
    CREATE TABLE IF NOT EXISTS networks (
        id          TEXT PRIMARY KEY NOT NULL,
        name        TEXT NOT NULL UNIQUE,
        description TEXT NOT NULL DEFAULT '',
        driver      TEXT NOT NULL DEFAULT 'bridge',
        is_default  INTEGER NOT NULL DEFAULT 0,
        created_at  TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_networks_name ON networks(name);
    INSERT OR IGNORE INTO networks (id, name, description, driver, is_default)
    VALUES ('default-pier-net', 'pier-net', 'Default network for all services', 'bridge', 1);
    ALTER TABLE services ADD COLUMN network_id TEXT REFERENCES networks(id) ON DELETE SET NULL;
    "#,
    // Migration 12: Canvas positions for architect view
    r#"
    CREATE TABLE IF NOT EXISTS canvas_positions (
        service_id  TEXT PRIMARY KEY REFERENCES services(id) ON DELETE CASCADE,
        x           REAL NOT NULL DEFAULT 0,
        y           REAL NOT NULL DEFAULT 0,
        updated_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );
    "#,
    // Migration 13: Server location + local server support
    r#"
    ALTER TABLE servers ADD COLUMN country TEXT;
    ALTER TABLE servers ADD COLUMN city TEXT;
    ALTER TABLE servers ADD COLUMN country_code TEXT;
    "#,
    // Migration 14: Project binding for git sources
    r#"
    ALTER TABLE git_sources ADD COLUMN project_id TEXT REFERENCES projects(id) ON DELETE SET NULL;
    "#,
    // Migration 15: Public port for Traefik TCP proxy
    r#"
    ALTER TABLE port_allocations ADD COLUMN public_port INTEGER;
    "#,
    // Migration 16: GitHub App manifest flow fields
    r#"
    ALTER TABLE git_sources ADD COLUMN webhook_secret TEXT;
    ALTER TABLE git_sources ADD COLUMN client_id TEXT;
    ALTER TABLE git_sources ADD COLUMN client_secret TEXT;
    "#,
    // Migration 17: Advanced service settings (auto_deploy, force_https)
    r#"
    ALTER TABLE services ADD COLUMN auto_deploy INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE services ADD COLUMN force_https INTEGER NOT NULL DEFAULT 1;
    "#,
    // Migration 18: Path prefix for domains (path-based routing)
    r#"
    ALTER TABLE domains ADD COLUMN path_prefix TEXT DEFAULT '';
    "#,
    // Migration 19: Store database credentials (username/password for created databases)
    r#"
    CREATE TABLE IF NOT EXISTS database_credentials (
        id          TEXT PRIMARY KEY NOT NULL,
        service_id  TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        db_name     TEXT NOT NULL,
        username    TEXT NOT NULL,
        password    TEXT NOT NULL,
        created_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_db_creds_service ON database_credentials(service_id);
    "#,
    // Migration 20: Multi-server support — server labels, deployments server_id
    r#"
    ALTER TABLE deployments ADD COLUMN server_id TEXT;
    ALTER TABLE servers ADD COLUMN labels_json TEXT DEFAULT '{}';
    ALTER TABLE servers ADD COLUMN max_containers INTEGER DEFAULT 100;
    "#,
    // Migration 21: Alerts & notifications (Phase 11.5)
    r#"
    CREATE TABLE IF NOT EXISTS alert_rules (
        id                  TEXT PRIMARY KEY NOT NULL,
        name                TEXT NOT NULL,
        enabled             INTEGER NOT NULL DEFAULT 1,
        metric              TEXT NOT NULL,
        scope               TEXT NOT NULL DEFAULT 'global',
        scope_id            TEXT,
        threshold           REAL,
        comparison          TEXT NOT NULL DEFAULT 'gt',
        duration_secs       INTEGER NOT NULL DEFAULT 60,
        severity            TEXT NOT NULL DEFAULT 'warning',
        channel             TEXT NOT NULL DEFAULT 'telegram',
        channel_config_enc  TEXT NOT NULL,
        cooldown_mins       INTEGER NOT NULL DEFAULT 30,
        last_triggered_at   TEXT,
        last_value          REAL,
        last_state          TEXT NOT NULL DEFAULT 'ok',
        first_breach_at     TEXT,
        created_at          TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at          TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_alert_rules_enabled ON alert_rules(enabled);

    CREATE TABLE IF NOT EXISTS alert_events (
        id               TEXT PRIMARY KEY NOT NULL,
        rule_id          TEXT NOT NULL REFERENCES alert_rules(id) ON DELETE CASCADE,
        state            TEXT NOT NULL,
        value            REAL,
        message          TEXT NOT NULL,
        delivered        INTEGER NOT NULL DEFAULT 0,
        delivery_error   TEXT,
        created_at       TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_alert_events_rule ON alert_events(rule_id, created_at DESC);
    "#,
    // Migration 22: Track "env changed but not redeployed" state per service
    r#"
    ALTER TABLE services ADD COLUMN env_dirty INTEGER NOT NULL DEFAULT 0;
    "#,
    // Migration 23: Simplified notifications — one channel config shared by presets.
    // UI shows on/off toggles for 10 predefined alerts; advanced rules still work via legacy endpoints.
    r#"
    CREATE TABLE IF NOT EXISTS notification_channels (
        channel    TEXT PRIMARY KEY NOT NULL,
        enabled    INTEGER NOT NULL DEFAULT 0,
        config_enc TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL DEFAULT (datetime('now'))
    );
    INSERT OR IGNORE INTO notification_channels (channel) VALUES ('telegram');

    INSERT OR IGNORE INTO alert_rules
        (id, name, enabled, metric, scope, threshold, comparison, duration_secs,
         severity, channel, channel_config_enc, cooldown_mins)
    VALUES
        ('preset-cpu-host',       'High server CPU',              0, 'cpu',              'global', 85.0, 'gt', 300,  'warning',  'telegram', '', 30),
        ('preset-ram-host',       'High server RAM',              0, 'ram',              'global', 85.0, 'gt', 300,  'warning',  'telegram', '', 30),
        ('preset-disk-host',      'High server disk usage',       0, 'disk',             'global', 90.0, 'gt', 300,  'warning',  'telegram', '', 60),
        ('preset-agent-offline',  'Remote server offline',        0, 'agent_offline',    'global', 5.0,  'gt', 0,    'critical', 'telegram', '', 30),
        ('preset-container-cpu',  'Container high CPU',           0, 'container_cpu',    'global', 90.0, 'gt', 300,  'warning',  'telegram', '', 30),
        ('preset-container-ram',  'Container high RAM',           0, 'container_ram',    'global', 90.0, 'gt', 300,  'warning',  'telegram', '', 30),
        ('preset-ssl-expiring',   'SSL certificate expiring',     0, 'ssl_expiry',       'global', 14.0, 'lt', 0,    'warning',  'telegram', '', 1440),
        ('preset-deploy-failed',  'Deployment failed',            0, 'deploy_status',    'global', NULL, 'eq', 0,    'critical', 'telegram', '', 5),
        ('preset-backup-failed',  'Backup failed',                0, 'backup_status',    'global', NULL, 'eq', 0,    'critical', 'telegram', '', 60),
        ('preset-container-down', 'Container crashed/restarting', 0, 'container_status', 'global', NULL, 'eq', 0,    'critical', 'telegram', '', 10);
    "#,
    // Migration 24: Seed additional channel rows (email functional, discord/slack stubs).
    // notification_channels schema is generic — no ALTER needed, just new rows.
    r#"
    INSERT OR IGNORE INTO notification_channels (channel) VALUES ('email'), ('discord'), ('slack');
    "#,
    // Migration 25: Additional preset alerts for success events + cleanup + reachability.
    r#"
    INSERT OR IGNORE INTO alert_rules
        (id, name, enabled, metric, scope, threshold, comparison, duration_secs,
         severity, channel, channel_config_enc, cooldown_mins)
    VALUES
        ('preset-deploy-success',   'Deployment succeeded',         0, 'deploy_success',          'global', NULL, 'eq', 0, 'info',     'telegram', '', 5),
        ('preset-backup-success',   'Backup succeeded',             0, 'backup_success',          'global', NULL, 'eq', 0, 'info',     'telegram', '', 60),
        ('preset-cleanup-success',  'Docker cleanup succeeded',     0, 'docker_cleanup_success',  'global', NULL, 'eq', 0, 'info',     'telegram', '', 720),
        ('preset-cleanup-failed',   'Docker cleanup failed',        0, 'docker_cleanup_failure',  'global', NULL, 'eq', 0, 'warning',  'telegram', '', 60),
        ('preset-server-reachable', 'Remote server back online',    0, 'server_reachable',        'global', NULL, 'eq', 0, 'info',     'telegram', '', 10);
    "#,
    // Migration 26: Registry credentials (per-project + global fallback).
    // Pulled/built images lookup creds by registry host; project-specific
    // entries override global ones for the same host.
    r#"
    CREATE TABLE IF NOT EXISTS registry_credentials (
        id           TEXT PRIMARY KEY NOT NULL,
        project_id   TEXT REFERENCES projects(id) ON DELETE CASCADE,
        registry     TEXT NOT NULL,
        username     TEXT NOT NULL,
        password_enc TEXT NOT NULL,
        label        TEXT,
        created_at   TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_regcreds_scope
        ON registry_credentials(COALESCE(project_id, ''), registry);
    CREATE INDEX IF NOT EXISTS idx_regcreds_project
        ON registry_credentials(project_id);
    "#,
    // Migration 27: Per-database backup schedules.
    // `database_name` NULL on backup_schedules = cluster-wide schedule (dumps
    // every DB in database_credentials as a tar archive). NULL on backups =
    // cluster-wide dump. A service can hold at most one schedule per
    // (service_id, database_name) pair; the unique index enforces this.
    r#"
    ALTER TABLE backup_schedules ADD COLUMN database_name TEXT;
    ALTER TABLE backups ADD COLUMN database_name TEXT;
    CREATE UNIQUE INDEX IF NOT EXISTS idx_backup_sched_service_db
        ON backup_schedules(service_id, COALESCE(database_name, ''));
    "#,
    // Migration 28: Load balancing — replica fan-out for stateless services.
    // `services.replicas` is the intended total count; `service_replicas`
    // holds actual placements (one row per running instance, possibly across
    // multiple servers). Separate from `cluster_config_json`, which is
    // reserved for DB primary/replica clusters.
    r#"
    ALTER TABLE services ADD COLUMN replicas INTEGER NOT NULL DEFAULT 1;
    ALTER TABLE services ADD COLUMN lb_strategy TEXT NOT NULL DEFAULT 'round-robin';
    ALTER TABLE services ADD COLUMN lb_sticky_cookie TEXT;

    CREATE TABLE IF NOT EXISTS service_replicas (
        id           TEXT PRIMARY KEY NOT NULL,
        service_id   TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
        server_id    TEXT REFERENCES servers(id) ON DELETE CASCADE,
        replica_idx  INTEGER NOT NULL,
        host_port    INTEGER NOT NULL,
        container_id TEXT,
        weight       INTEGER NOT NULL DEFAULT 1,
        status       TEXT NOT NULL DEFAULT 'pending',
        created_at   TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at   TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_replicas_slot
        ON service_replicas(service_id, COALESCE(server_id, ''), replica_idx);
    CREATE INDEX IF NOT EXISTS idx_replicas_service ON service_replicas(service_id);
    CREATE INDEX IF NOT EXISTS idx_replicas_server  ON service_replicas(server_id);

    INSERT INTO service_replicas (id, service_id, server_id, replica_idx, host_port, container_id, status)
    SELECT
        lower(hex(randomblob(16))),
        s.id,
        s.server_id,
        1,
        COALESCE(s.port, 0),
        s.container_id,
        s.status
    FROM services s
    WHERE NOT EXISTS (
        SELECT 1 FROM service_replicas r WHERE r.service_id = s.id
    );
    "#,
    // Migration 29: Enforce unique service name within (project_id) scope.
    // Pre-dedupe: keep newest row per (project_id, name); failed orphans are safe to drop.
    r#"
    DELETE FROM services
    WHERE id IN (
        SELECT s.id FROM services s
        JOIN (
            SELECT COALESCE(project_id,'') AS pid, name, MAX(created_at) AS max_ts
            FROM services
            GROUP BY COALESCE(project_id,''), name
            HAVING COUNT(*) > 1
        ) dup
          ON COALESCE(s.project_id,'') = dup.pid AND s.name = dup.name
        WHERE s.created_at < dup.max_ts
    );

    CREATE UNIQUE INDEX IF NOT EXISTS idx_services_name_scope
        ON services(COALESCE(project_id,''), name);
    "#,
    // Migration 30: Core↔Core federation (Mode 2 "pier-core → pier-core").
    // peer_cores  — peers this instance CAN CONTROL (outgoing).
    // peer_grants — tokens this instance accepts FROM remote cores (incoming).
    r#"
    CREATE TABLE IF NOT EXISTS peer_cores (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL,
        url             TEXT NOT NULL,
        api_token       TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'pending',
        last_heartbeat  TEXT,
        remote_version  TEXT,
        last_error      TEXT,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_peer_cores_status ON peer_cores(status);

    CREATE TABLE IF NOT EXISTS peer_grants (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL,
        token           TEXT NOT NULL UNIQUE,
        is_active       INTEGER NOT NULL DEFAULT 1,
        last_used_at    TEXT,
        last_used_ip    TEXT,
        created_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_peer_grants_token ON peer_grants(token);
    "#,
    // Migration 31: Unify peer_cores into servers via a `kind` discriminator.
    // Rationale: UX-wise the two are "remote infrastructure I control" —
    // keeping them in separate tables meant two of everything (pages, forms,
    // heartbeat tasks, proxies). After this migration `servers.kind` is the
    // single source of truth; `is_local` stays as a convenience flag used
    // by older queries.
    r#"
    ALTER TABLE servers ADD COLUMN kind TEXT NOT NULL DEFAULT 'agent';
    ALTER TABLE servers ADD COLUMN url TEXT;
    ALTER TABLE servers ADD COLUMN remote_version TEXT;
    ALTER TABLE servers ADD COLUMN last_error TEXT;

    UPDATE servers SET kind = 'local' WHERE is_local = 1;

    -- Copy every peer_cores row into servers with kind='peer'.
    -- For peers: host is left empty (url is the real address), port 0.
    -- The api_token from peer_cores lands in servers.agent_token — same column,
    -- different semantics per kind (Bearer for agents, X-Pier-Peer-Token for peers).
    INSERT OR IGNORE INTO servers
        (id, name, host, port, agent_token, status, last_heartbeat,
         kind, url, remote_version, last_error, is_local, created_at, updated_at)
    SELECT
        id, name, '', 0, api_token, status, last_heartbeat,
        'peer', url, remote_version, last_error, 0, created_at, updated_at
    FROM peer_cores;

    DROP TABLE IF EXISTS peer_cores;

    CREATE INDEX IF NOT EXISTS idx_servers_kind ON servers(kind);
    "#,
    // Migration 32: per-compose-service tagging for port_allocations and domains.
    // A single Pier resource can deploy a docker-compose with N services, each
    // needing its own domain. NULL = legacy single-service (current behavior);
    // non-NULL = the YAML key under `services:` this row belongs to.
    r#"
    ALTER TABLE port_allocations ADD COLUMN compose_service TEXT;
    ALTER TABLE domains ADD COLUMN compose_service TEXT;
    CREATE INDEX IF NOT EXISTS idx_port_alloc_compose_svc
        ON port_allocations(service_id, compose_service);
    CREATE INDEX IF NOT EXISTS idx_domains_compose_svc
        ON domains(service_id, compose_service);
    "#,
    // Migration 33: Per-storage key prefix for backup S3 keys.
    // Lets the operator pick the first folder under the bucket (server name,
    // project label, anything). Default keeps the historical "pier-backups"
    // prefix so existing storages keep writing to the same paths.
    r#"
    ALTER TABLE s3_storages ADD COLUMN key_prefix TEXT NOT NULL DEFAULT 'pier-backups';
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
            conn.execute("INSERT INTO _migrations (version) VALUES (?1)", [version])?;
        }
    }

    Ok(())
}
