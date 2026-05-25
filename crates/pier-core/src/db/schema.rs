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
    // Migration 34: Embedded npm registry — packages, versions, and Bearer API tokens.
    //
    // npm_packages: one row per package (private or proxy-cached). dist_tags_json
    // is the canonical map (`{"latest":"1.2.3"}`). is_proxy=1 means this row
    // mirrors a public upstream and gets refreshed via upstream_fetched_at TTL.
    //
    // npm_versions: one row per (package, version). manifest_json holds the full
    // version manifest as published; tarball_sha512 is the integrity hash.
    // s3_uploaded flips to 1 once the cold-tier upload to S3 succeeds.
    //
    // api_tokens: Bearer tokens (sha256-hashed at rest, like GitHub PATs).
    // The plaintext token is shown to the user once at creation and never again.
    // `prefix` lets the UI show "pier_npm_a1b2…" without revealing the secret.
    r#"
    CREATE TABLE IF NOT EXISTS npm_packages (
        name                  TEXT PRIMARY KEY NOT NULL,
        description           TEXT NOT NULL DEFAULT '',
        dist_tags_json        TEXT NOT NULL DEFAULT '{}',
        is_proxy              INTEGER NOT NULL DEFAULT 0,
        upstream_etag         TEXT,
        upstream_fetched_at   INTEGER,
        created_at            INTEGER NOT NULL,
        updated_at            INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_npm_packages_is_proxy ON npm_packages(is_proxy);

    CREATE TABLE IF NOT EXISTS npm_versions (
        package_name    TEXT NOT NULL,
        version         TEXT NOT NULL,
        manifest_json   TEXT NOT NULL,
        tarball_size    INTEGER NOT NULL,
        tarball_sha512  TEXT NOT NULL,
        s3_uploaded     INTEGER NOT NULL DEFAULT 0,
        published_by    TEXT,
        published_at    INTEGER NOT NULL,
        PRIMARY KEY (package_name, version),
        FOREIGN KEY (package_name) REFERENCES npm_packages(name) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_npm_versions_package ON npm_versions(package_name);
    CREATE INDEX IF NOT EXISTS idx_npm_versions_s3 ON npm_versions(s3_uploaded);

    CREATE TABLE IF NOT EXISTS api_tokens (
        id            TEXT PRIMARY KEY NOT NULL,
        user_id       TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        name          TEXT NOT NULL,
        token_hash    TEXT NOT NULL UNIQUE,
        prefix        TEXT NOT NULL,
        last_used_at  INTEGER,
        created_at    INTEGER NOT NULL,
        revoked_at    INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_api_tokens_user ON api_tokens(user_id);
    CREATE INDEX IF NOT EXISTS idx_api_tokens_hash ON api_tokens(token_hash);
    "#,
    // Migration 35: Per-user TOTP (2FA) state.
    // totp_secret holds the AES-256-encrypted base32 secret ("ENC:…" via
    // `crypto::encrypt`); NULL means 2FA disabled for that user.
    // totp_recovery_codes is a JSON array of SHA-256 hex hashes — codes are
    // displayed once at enrollment and never recoverable. totp_enabled_at is
    // audit metadata (when the user finalised enrollment).
    r#"
    ALTER TABLE users ADD COLUMN totp_secret TEXT;
    ALTER TABLE users ADD COLUMN totp_recovery_codes TEXT;
    ALTER TABLE users ADD COLUMN totp_enabled_at TEXT;
    "#,
    // Migration 36: Auth audit log — `auth_events` records every credential /
    // session action so operators can answer "who, when, from which IP" after
    // an incident. Retention is enforced by a daily background task (see
    // `auth::audit::retention_sweep`) with thresholds in `settings`
    // (`audit.retention_days` / `audit.retention_days_sensitive`).
    r#"
    CREATE TABLE IF NOT EXISTS auth_events (
        id          TEXT PRIMARY KEY NOT NULL,
        user_id     TEXT,
        event_type  TEXT NOT NULL,
        ip          TEXT,
        user_agent  TEXT,
        details     TEXT,
        created_at  TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_auth_events_user_id ON auth_events(user_id, created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_auth_events_type    ON auth_events(event_type, created_at DESC);
    CREATE INDEX IF NOT EXISTS idx_auth_events_ip      ON auth_events(ip, created_at DESC);
    "#,
    // Migration 37: Tombstone column for unpublished npm packages.
    //
    // When `npm unpublish <pkg>` removes every version we keep the row around
    // with `unpublished_at` set, so `GET /{pkg}` returns 404 (not the cached
    // empty packument with stale dist-tags). The npm spec considers a 24h
    // re-publish window after unpublish — we encode that policy here later if
    // needed; today the row is permanent unless the operator deletes it.
    r#"
    ALTER TABLE npm_packages ADD COLUMN unpublished_at INTEGER;
    CREATE INDEX IF NOT EXISTS idx_npm_packages_unpublished_at
        ON npm_packages(unpublished_at)
        WHERE unpublished_at IS NOT NULL;
    "#,
    // Migration 38: `npm login --auth-type=web` session store.
    //
    // The CLI POSTs /-/v1/login, gets a session id back, and opens the
    // matching panel URL in a browser. The panel authorises the session (with
    // 2FA), at which point we mint an api_token and link it via `token_id`.
    // The CLI polls /-/v1/done/{id}, sees the token, and writes it to .npmrc.
    //
    // `status` is `pending` | `authorized` | `expired` so the polling endpoint
    // can return the right thing per state.
    //
    // `token_plaintext_enc` is the freshly-minted token encrypted via
    // `crate::crypto` so the CLI can fetch it on its next poll — `api_tokens`
    // only ever holds the hash. We blank this column the moment the CLI
    // consumes it (defence-in-depth: short-lived row, short-lived secret).
    r#"
    CREATE TABLE IF NOT EXISTS npm_login_sessions (
        session_id           TEXT PRIMARY KEY NOT NULL,
        hostname             TEXT NOT NULL DEFAULT '',
        status               TEXT NOT NULL DEFAULT 'pending',
        token_id             TEXT REFERENCES api_tokens(id) ON DELETE SET NULL,
        token_plaintext_enc  TEXT,
        peer_ip              TEXT,
        user_agent           TEXT,
        created_at           INTEGER NOT NULL,
        expires_at           INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_npm_login_sessions_expires
        ON npm_login_sessions(expires_at);
    "#,
    // Migration 39: Per-domain `strip_prefix` toggle (Coolify-style).
    //
    // When a domain is attached with a path (`example.com/api`) Pier emits a
    // Traefik `stripPrefix` middleware so the upstream sees `/` instead of
    // `/api`. That's the right default for proxied APIs, but breaks backends
    // whose own router is registered at the same prefix (e.g. a bot listening
    // on `POST /webhook`).
    //
    // strip_prefix = 1 (default) preserves current behavior; 0 keeps the
    // prefix in the forwarded request. The flag is ignored for domains that
    // have no path (nothing to strip).
    r#"
    ALTER TABLE domains ADD COLUMN strip_prefix INTEGER NOT NULL DEFAULT 1;
    "#,
    // Migration 40: Bootstrap tokens (TTL'd) + hashed long-term agent tokens.
    //
    // Previously `servers.agent_token` held a plaintext secret that doubled as
    // both the install-time bootstrap credential and the long-lived API token.
    // A DB leak therefore handed over full control of every connected node.
    //
    // New flow (used by rows inserted after this migration):
    //   1. Operator creates a server in the UI → core issues a short-lived
    //      bootstrap token (TTL 1h), stores only its sha256 in
    //      `bootstrap_token_hash`.
    //   2. install.sh ships the bootstrap plaintext as `PIER_BOOTSTRAP_TOKEN`.
    //   3. Agent on first boot POSTs `/api/v1/servers/{id}/handshake` with the
    //      bootstrap; core generates a long-term token, hashes it into
    //      `agent_token_hash`, returns plaintext exactly once, and clears the
    //      bootstrap columns.
    //   4. All subsequent calls (heartbeat, outbound Bearer) carry the
    //      long-term plaintext, validated via sha256 compare on the agent and
    //      via `agent_token_hash` lookup on core.
    //
    // Backward compatibility: existing rows keep their plaintext `agent_token`
    // populated; the heartbeat validator falls back to a plaintext match when
    // `agent_token_hash IS NULL`, and lazily backfills the hash on first
    // successful match. A later migration can null out the legacy column
    // once telemetry confirms all agents have rotated.
    //
    // `agent_token_prefix` mirrors the api_tokens pattern (auth/api_token.rs):
    // a short visible fingerprint shown in the UI so the operator can
    // distinguish tokens without exposing them.
    r#"
    ALTER TABLE servers ADD COLUMN bootstrap_token_hash TEXT;
    ALTER TABLE servers ADD COLUMN bootstrap_expires_at INTEGER;
    ALTER TABLE servers ADD COLUMN agent_token_hash TEXT;
    ALTER TABLE servers ADD COLUMN agent_token_prefix TEXT;
    CREATE INDEX IF NOT EXISTS idx_servers_bootstrap_hash
        ON servers(bootstrap_token_hash)
        WHERE bootstrap_token_hash IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_servers_agent_token_hash
        ON servers(agent_token_hash)
        WHERE agent_token_hash IS NOT NULL;
    "#,
    // Migration 41: WireGuard mesh tables.
    //
    // `wireguard_config` is a singleton (CHECK id = 1) holding the
    // operator's choice of subnet, listen port, and keepalive. It exists
    // even when mesh is disabled so the UI has somewhere to read the
    // defaults from before Enable Mesh is pressed.
    //
    // `wireguard_peers` holds one row per participating server (including
    // the local node — assigned_ip there describes core's own wg0
    // address). Lifecycle:
    //
    //   * created with `status='pending'` and `public_key IS NULL` when
    //     Enable Mesh first assigns the row an IP;
    //   * core asks the node's pier-net-helper to generate a keypair,
    //     receives the public_key, and persists it (status='keyed');
    //   * core renders the final wg0.conf, asks the helper to apply it,
    //     and on successful `wg syncconf` flips status to 'active';
    //   * status='error' carries the helper's last error_message for
    //     UI display.
    //
    // assigned_ip is UNIQUE — the IP allocator hands out the lowest free
    // /32 inside `wireguard_config.subnet`, so adding/removing nodes
    // doesn't reshuffle existing tunnels.
    r#"
    CREATE TABLE IF NOT EXISTS wireguard_config (
        id                    INTEGER PRIMARY KEY CHECK (id = 1),
        enabled               INTEGER NOT NULL DEFAULT 0,
        subnet                TEXT    NOT NULL DEFAULT '10.42.0.0/24',
        listen_port           INTEGER NOT NULL DEFAULT 51820,
        persistent_keepalive  INTEGER NOT NULL DEFAULT 25,
        updated_at            INTEGER NOT NULL DEFAULT (strftime('%s','now'))
    );
    INSERT OR IGNORE INTO wireguard_config (id, updated_at)
        VALUES (1, strftime('%s','now'));

    CREATE TABLE IF NOT EXISTS wireguard_peers (
        server_id       TEXT PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
        assigned_ip     TEXT NOT NULL UNIQUE,
        public_key      TEXT,
        endpoint        TEXT NOT NULL,
        last_handshake  INTEGER,
        rx_bytes        INTEGER NOT NULL DEFAULT 0,
        tx_bytes        INTEGER NOT NULL DEFAULT 0,
        status          TEXT NOT NULL DEFAULT 'pending',
        error_message   TEXT,
        deployed_at     INTEGER,
        created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
    );
    CREATE INDEX IF NOT EXISTS idx_wireguard_peers_status
        ON wireguard_peers(status);
    "#,
    // Migration 42: per-server token rotation tracking.
    //
    // The bootstrap → long-term handshake from migration 40 hashed the
    // server credential at rest, but the token itself never expires
    // until the operator manually deletes the server. For a long-lived
    // mesh that's a leak vector — a stolen agent_token (e.g. via a
    // compromised systemd unit file) lets the attacker pose as that
    // node forever.
    //
    // `token_rotated_at` is the wall-clock timestamp of the most
    // recent successful rotation. NULL means "never rotated since
    // initial handshake" — treat it like `created_at` for scheduling.
    // `token_version` is a monotonic counter the UI shows so the
    // operator can see history at a glance and so a /rotate that
    // races with a second click is detectable.
    r#"
    ALTER TABLE servers ADD COLUMN token_rotated_at INTEGER;
    ALTER TABLE servers ADD COLUMN token_version INTEGER NOT NULL DEFAULT 1;
    "#,
    // Migration 43: Federation read-only cache.
    //
    // The primary core polls every peer-kind server on a 30-60s timer and
    // upserts the snapshot of its projects/stacks here so the dashboard can
    // render a merged "all nodes" view without doing N synchronous HTTP
    // calls on every page load. Rows are write-mostly: the scheduler
    // `DELETE … WHERE peer_server_id = ?` then re-INSERTs the full set
    // on each successful poll, so the table is the *current* snapshot,
    // not history. `fetched_at` is what the UI uses to flag stale data.
    //
    // We carry the *peer's own* project_id / stack_id as the secondary
    // key, never our local UUIDs — those would collide and would imply
    // we own the row. The (peer_server_id, project_id|stack_id) primary
    // key is what lets the upserter avoid duplicates without a separate
    // surrogate key.
    r#"
    CREATE TABLE IF NOT EXISTS federated_projects (
        peer_server_id  TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        project_id      TEXT NOT NULL,
        name            TEXT NOT NULL,
        description     TEXT NOT NULL DEFAULT '',
        services_count  INTEGER NOT NULL DEFAULT 0,
        fetched_at      INTEGER NOT NULL,
        PRIMARY KEY (peer_server_id, project_id)
    );
    CREATE INDEX IF NOT EXISTS idx_federated_projects_peer
        ON federated_projects(peer_server_id);

    CREATE TABLE IF NOT EXISTS federated_stacks (
        peer_server_id  TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        stack_id        TEXT NOT NULL,
        name            TEXT NOT NULL,
        status          TEXT NOT NULL DEFAULT 'unknown',
        has_yaml        INTEGER NOT NULL DEFAULT 0,
        fetched_at      INTEGER NOT NULL,
        PRIMARY KEY (peer_server_id, stack_id)
    );
    CREATE INDEX IF NOT EXISTS idx_federated_stacks_peer
        ON federated_stacks(peer_server_id);

    -- Per-peer sync bookkeeping; one row per peer regardless of state.
    -- last_error is null on success, NOT NULL after a failed pass.
    CREATE TABLE IF NOT EXISTS federation_sync_state (
        peer_server_id  TEXT PRIMARY KEY REFERENCES servers(id) ON DELETE CASCADE,
        last_synced_at  INTEGER,
        last_attempt_at INTEGER,
        last_status     TEXT NOT NULL DEFAULT 'pending',
        last_error      TEXT,
        consecutive_failures INTEGER NOT NULL DEFAULT 0
    );
    "#,
    // Migration 44: Multi-user RBAC — global role on users.
    //
    // Until now `users.role` was a free-form TEXT used only as `== "admin"`
    // in a handful of places. We're moving to a typed three-level hierarchy
    // (Owner / Admin / User) but keep the legacy column populated so a
    // rollback to N-1 still parses sessions correctly.
    //
    // The first user by `created_at` is promoted to Owner (the installer);
    // any other admins become global Admin. Everyone else stays at User.
    r#"
    ALTER TABLE users ADD COLUMN global_role TEXT NOT NULL DEFAULT 'user';

    UPDATE users SET global_role = 'owner'
        WHERE id = (
            SELECT id FROM users WHERE role = 'admin'
            ORDER BY created_at ASC LIMIT 1
        );

    UPDATE users SET global_role = 'admin'
        WHERE role = 'admin' AND global_role = 'user';

    CREATE INDEX IF NOT EXISTS idx_users_global_role ON users(global_role);
    "#,
    // Migration 45: Project-scoped membership.
    //
    // One row per (user, project) granting one of three project roles:
    // `admin` (manages membership + everything below), `editor` (deploy,
    // edit env, restart), `viewer` (read-only). Global Owner/Admin bypass
    // these rows entirely — they're for granting non-admin users access
    // to specific projects.
    //
    // UNIQUE(project_id, user_id) prevents accidental duplicates; cascade
    // deletes from either side clear the row automatically.
    r#"
    CREATE TABLE IF NOT EXISTS project_members (
        id           TEXT PRIMARY KEY NOT NULL,
        project_id   TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
        user_id      TEXT NOT NULL REFERENCES users(id)    ON DELETE CASCADE,
        project_role TEXT NOT NULL CHECK (project_role IN ('admin','editor','viewer')),
        added_at     TEXT NOT NULL DEFAULT (datetime('now')),
        added_by     TEXT REFERENCES users(id) ON DELETE SET NULL,
        UNIQUE(project_id, user_id)
    );
    CREATE INDEX IF NOT EXISTS idx_project_members_user
        ON project_members(user_id);
    CREATE INDEX IF NOT EXISTS idx_project_members_project
        ON project_members(project_id);
    "#,
    // Migration 46: User invitations — one-time tokens for onboarding.
    //
    // Admins generate an invite for an email + default global role. The
    // plaintext token is shown once to the inviter (copy-paste link); we
    // only store sha256 here, same pattern as `api_tokens`. The invitee
    // opens /invitations/{token}, sets a password (and optional 2FA),
    // and the row flips to `accepted_at IS NOT NULL` with a back-link to
    // the new user row via `accepted_user_id`.
    //
    // Expired rows are kept for audit but cannot be redeemed (handler
    // checks `expires_at > now() AND accepted_at IS NULL`).
    r#"
    CREATE TABLE IF NOT EXISTS user_invitations (
        id                  TEXT PRIMARY KEY NOT NULL,
        email               TEXT NOT NULL,
        invite_token_hash   TEXT NOT NULL UNIQUE,
        default_global_role TEXT NOT NULL DEFAULT 'user',
        invited_by          TEXT REFERENCES users(id) ON DELETE SET NULL,
        expires_at          TEXT NOT NULL,
        accepted_at         TEXT,
        accepted_user_id    TEXT REFERENCES users(id) ON DELETE SET NULL,
        created_at          TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_user_invitations_email
        ON user_invitations(email);
    CREATE INDEX IF NOT EXISTS idx_user_invitations_expires_at
        ON user_invitations(expires_at);
    "#,
    // Migration 47: Ad-hoc Tasks — saved shell command "templates" plus a
    // history of every run.
    //
    // `task_templates` is the Semaphore-style "saved task" — a named
    // command + default timeout. Templates can be invoked one-click, or
    // an ad-hoc run can skip the template entirely (template_id NULL).
    //
    // `task_runs` records each execution. `command_snapshot` captures the
    // exact command that ran (so editing a template later doesn't rewrite
    // history). `agent_run_id` is the in-memory id the agent assigns —
    // we use it to re-attach to the agent's buffer after a core restart.
    // `stdout` / `stderr` are kept inline (capped to 5 MiB in handler)
    // for simplicity; if the deployment-log pattern outgrows TEXT we
    // can split to a side table later.
    r#"
    CREATE TABLE IF NOT EXISTS task_templates (
        id                   TEXT PRIMARY KEY NOT NULL,
        name                 TEXT NOT NULL,
        description          TEXT,
        command              TEXT NOT NULL,
        default_timeout_sec  INTEGER NOT NULL DEFAULT 1800,
        default_env_json     TEXT NOT NULL DEFAULT '{}',
        created_by           TEXT NOT NULL,
        created_at           TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at           TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE UNIQUE INDEX IF NOT EXISTS idx_task_templates_name
        ON task_templates(name);

    CREATE TABLE IF NOT EXISTS task_runs (
        id               TEXT PRIMARY KEY NOT NULL,
        template_id      TEXT REFERENCES task_templates(id) ON DELETE SET NULL,
        server_id        TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        batch_id         TEXT,
        command_snapshot TEXT NOT NULL,
        env_json         TEXT NOT NULL DEFAULT '{}',
        timeout_sec      INTEGER NOT NULL,
        status           TEXT NOT NULL,
        exit_code        INTEGER,
        stdout           TEXT NOT NULL DEFAULT '',
        stderr           TEXT NOT NULL DEFAULT '',
        agent_run_id     TEXT,
        triggered_by     TEXT NOT NULL,
        error_message    TEXT,
        started_at       TEXT NOT NULL DEFAULT (datetime('now')),
        finished_at      TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_task_runs_server
        ON task_runs(server_id, started_at DESC);
    CREATE INDEX IF NOT EXISTS idx_task_runs_status
        ON task_runs(status, started_at DESC);
    CREATE INDEX IF NOT EXISTS idx_task_runs_template
        ON task_runs(template_id, started_at DESC);
    CREATE INDEX IF NOT EXISTS idx_task_runs_batch
        ON task_runs(batch_id);
    "#,
    // Migration 48: User-defined cron schedules.
    //
    // `schedules` is the single source of truth for "fire X on a cron".
    // It replaces the per-feature loops (backup scheduler, Docker cleanup
    // loop) with one tokio task that reads from this table. `action_type`
    // selects the dispatcher; `action_config` is its JSON payload schema:
    //
    //   * `task`    — `{template_id: "...", server_id: "..."}`
    //   * `backup`  — `{backup_schedule_id: "..."}` (joins backup_schedules
    //                 for the actual cron + service binding; the row here
    //                 only tracks when it's due next)
    //   * `cleanup` — `{prune_images: bool, prune_cache: bool,
    //                  prune_containers: bool}`
    //
    // `is_system = 1` marks rows seeded by core itself (auto-cleanup,
    // backed-up legacy schedules). UI hides their Delete button but
    // allows Enable/Disable.
    //
    // `schedule_runs` records every fire — manually triggered, cron-driven,
    // or skipped due to concurrency / misfire. `task_run_id` links to
    // `task_runs` for `action_type='task'` so the user can drill from
    // the schedule history into the live task log.
    r#"
    CREATE TABLE IF NOT EXISTS schedules (
        id              TEXT PRIMARY KEY NOT NULL,
        name            TEXT NOT NULL,
        description     TEXT NOT NULL DEFAULT '',
        cron_expression TEXT NOT NULL,
        timezone        TEXT NOT NULL DEFAULT 'UTC',
        action_type     TEXT NOT NULL,
        action_config   TEXT NOT NULL DEFAULT '{}',
        enabled         INTEGER NOT NULL DEFAULT 1,
        misfire_policy  TEXT NOT NULL DEFAULT 'skip',
        last_run_at     TEXT,
        next_run_at     TEXT,
        last_status     TEXT,
        last_error      TEXT,
        created_by      TEXT REFERENCES users(id) ON DELETE SET NULL,
        is_system       INTEGER NOT NULL DEFAULT 0,
        created_at      TEXT NOT NULL DEFAULT (datetime('now')),
        updated_at      TEXT NOT NULL DEFAULT (datetime('now'))
    );
    CREATE INDEX IF NOT EXISTS idx_schedules_due
        ON schedules(enabled, next_run_at);
    CREATE INDEX IF NOT EXISTS idx_schedules_action_type
        ON schedules(action_type);

    CREATE TABLE IF NOT EXISTS schedule_runs (
        id            TEXT PRIMARY KEY NOT NULL,
        schedule_id   TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
        started_at    TEXT NOT NULL DEFAULT (datetime('now')),
        finished_at   TEXT,
        status        TEXT NOT NULL DEFAULT 'running',
        triggered_by  TEXT NOT NULL DEFAULT 'cron',
        output        TEXT NOT NULL DEFAULT '',
        error         TEXT,
        task_run_id   TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_schedule_runs_sched
        ON schedule_runs(schedule_id, started_at DESC);

    -- Backfill: every active row in backup_schedules becomes a `backup`
    -- schedule. The unified runner reads cron_expression from
    -- backup_schedules (via action_config.backup_schedule_id), so this
    -- preserves the legacy timer behavior verbatim.
    INSERT INTO schedules
        (id, name, cron_expression, timezone, action_type, action_config,
         enabled, is_system)
    SELECT
        'sched-bk-' || bs.id,
        'Backup: ' || COALESCE(s.name, bs.service_id),
        bs.cron_expression,
        'UTC',
        'backup',
        json_object('backup_schedule_id', bs.id),
        bs.is_active,
        1
    FROM backup_schedules bs
    LEFT JOIN services s ON s.id = bs.service_id
    WHERE NOT EXISTS (
        SELECT 1 FROM schedules WHERE id = 'sched-bk-' || bs.id
    );
    "#,
    // Migration 49: Seed the system Docker-cleanup schedule.
    //
    // Replaces the standalone tokio::spawn loop in main.rs. We translate
    // the existing `cleanup.interval_hours` setting into a cron expression
    // so operators don't lose their cadence: 24h → '0 0 * * *', 12h →
    // '0 */12 * * *', 6h → '0 */6 * * *', 1h → '0 * * * *'. Anything else
    // falls back to '0 0 * * *' (daily at midnight UTC).
    //
    // `enabled` mirrors the previous `cleanup.enabled` setting. Prune
    // flags land in `action_config` so the cleanup dispatcher can read
    // them without re-querying `settings` on every fire. `is_system = 1`
    // protects the row from accidental deletion in the UI — operators
    // can still disable or re-tune it.
    //
    // INSERT OR IGNORE keeps the migration idempotent: if a previous
    // partial deploy already inserted the row, we don't clobber it.
    r#"
    INSERT OR IGNORE INTO schedules
        (id, name, description, cron_expression, timezone, action_type,
         action_config, enabled, is_system)
    SELECT
        'sched-cleanup-default',
        'Docker cleanup',
        'Periodically prune unused Docker images, build cache, and (optionally) containers.',
        CASE
            WHEN COALESCE((SELECT value FROM settings WHERE key='cleanup.interval_hours'), '24') = '1'  THEN '0 * * * *'
            WHEN COALESCE((SELECT value FROM settings WHERE key='cleanup.interval_hours'), '24') = '6'  THEN '0 */6 * * *'
            WHEN COALESCE((SELECT value FROM settings WHERE key='cleanup.interval_hours'), '24') = '12' THEN '0 */12 * * *'
            WHEN COALESCE((SELECT value FROM settings WHERE key='cleanup.interval_hours'), '24') = '48' THEN '0 0 */2 * *'
            WHEN COALESCE((SELECT value FROM settings WHERE key='cleanup.interval_hours'), '24') = '168' THEN '0 0 * * 0'
            ELSE '0 0 * * *'
        END,
        'UTC',
        'cleanup',
        json_object(
            'prune_images',      COALESCE((SELECT value FROM settings WHERE key='cleanup.prune_images'),      'true') != 'false',
            'prune_build_cache', COALESCE((SELECT value FROM settings WHERE key='cleanup.prune_build_cache'), 'true') != 'false',
            'prune_containers',  COALESCE((SELECT value FROM settings WHERE key='cleanup.prune_containers'),  'false') = 'true'
        ),
        CASE
            WHEN COALESCE((SELECT value FROM settings WHERE key='cleanup.enabled'), 'true') = 'false' THEN 0
            ELSE 1
        END,
        1;
    "#,
    // Migration 50: Re-label backfilled backup schedules so per-database
    // and cluster-wide rows are visually distinct.
    //
    // Migration 48 created one mirror row per `backup_schedules` entry but
    // composed the display label from `service_name` only — services that
    // have 5+ schedules (cluster dump + per-DB dumps) all ended up as
    // "Backup: postgresql" in the UI. This pass appends `(cluster)` or
    // `(<database_name>)` so operators can tell them apart at a glance.
    //
    // Only touches rows the backfill owns (`id LIKE 'sched-bk-%'` AND
    // `action_type = 'backup'`); user-created schedules and the cleanup
    // row are untouched.
    r#"
    UPDATE schedules
       SET name = (
           SELECT 'Backup: ' || COALESCE(s.name, bs.service_id)
                  || CASE
                       WHEN bs.database_name IS NOT NULL AND bs.database_name <> ''
                       THEN ' (' || bs.database_name || ')'
                       ELSE ' (cluster)'
                     END
             FROM backup_schedules bs
             LEFT JOIN services s ON s.id = bs.service_id
            WHERE 'sched-bk-' || bs.id = schedules.id
       )
     WHERE action_type = 'backup'
       AND id LIKE 'sched-bk-%'
       AND EXISTS (
           SELECT 1 FROM backup_schedules bs
            WHERE 'sched-bk-' || bs.id = schedules.id
       );
    "#,
    // Migration 51: Write-federation foundation (Etap 2.1).
    //
    // Peer pier-core can now accept a long-lived `federation_token` from
    // a remote primary pier-core. Token plaintext lives only in the
    // operator's clipboard during pairing; we keep just its SHA-256 hash
    // and an 8-char prefix for the UI. Schema mirrors `peer_grants`
    // (migration 30) but for the opposite direction — peer GRANTING a
    // primary write access, vs peer-cores reading from each other.
    //
    // Why a separate table and not an extension of `peer_grants`:
    // - `peer_grants` carries the full plaintext token (legacy reasons).
    //   New surface gets to start clean with hashing-only.
    // - Audit trail per-token (last_used_at) for revocation hygiene
    //   needs its own column set.
    // - Federation tokens grant a strictly narrower scope (only
    //   `/api/v1/agent/*`, no UI sessions), so mixing them with the
    //   broad-scope peer_grants would be a foot-gun.
    //
    // Ownership tracking: each `services` (compose stack) and `projects`
    // row gains an `owner_server_id` column. NULL means "managed by this
    // peer's own UI" — the legacy case for every pre-existing row.
    // Non-NULL stores `federation_tokens.id`, so we can attribute a
    // stack to "the primary identified by token X". Tokens are
    // groupable later (a future migration could add a `primary_id`
    // grouping if we let one primary rotate through multiple tokens),
    // but MVP keeps it 1:1.
    r#"
    CREATE TABLE IF NOT EXISTS federation_tokens (
        id              TEXT PRIMARY KEY NOT NULL,
        token_hash      TEXT NOT NULL UNIQUE,
        token_prefix    TEXT NOT NULL,
        label           TEXT NOT NULL,
        is_active       INTEGER NOT NULL DEFAULT 1,
        created_at      INTEGER NOT NULL,
        last_used_at    INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_federation_tokens_hash
        ON federation_tokens(token_hash);
    CREATE INDEX IF NOT EXISTS idx_federation_tokens_active
        ON federation_tokens(is_active);

    ALTER TABLE services ADD COLUMN owner_server_id TEXT;
    ALTER TABLE projects ADD COLUMN owner_server_id TEXT;

    CREATE INDEX IF NOT EXISTS idx_services_owner_server_id
        ON services(owner_server_id);
    CREATE INDEX IF NOT EXISTS idx_projects_owner_server_id
        ON projects(owner_server_id);
    "#,
    // Migration 52: Primary-side federation_token storage (Etap 2.4).
    //
    // Migration 51 added the *peer-side* federation_tokens table — that's
    // where a peer pier-core stores the SHA-256 hash of the token it
    // minted for a particular primary. THIS migration is the other end of
    // the wire: the primary remembers the plaintext federation_token it
    // received during pairing so the federation write-client can present
    // it on every primary→peer call.
    //
    // Plaintext at rest is the same trade-off `servers.agent_token`
    // already makes for outbound heartbeat auth. A future migration can
    // promote this to "encrypted by a master secret in data_dir" once
    // the same is done for agent_token (see deferred items in Etap 0.5).
    r#"
    ALTER TABLE servers ADD COLUMN federation_token TEXT;
    "#,
    // Migration 53: Federation-token rotation timestamp (Etap 2.9).
    //
    // The agent-token rotator (Etap 0.4 / migration 42) keys off
    // servers.token_rotated_at. Reusing that column for federation
    // tokens would conflate two unrelated rotation cycles on the same
    // row, so we add a parallel column dedicated to federation. NULL
    // means "never rotated" — treat it as "rotate-on-first-tick after
    // pairing", same semantics as token_rotated_at NULL for agents.
    r#"
    ALTER TABLE servers ADD COLUMN federation_token_rotated_at INTEGER;
    "#,
    // Migration 54: Mesh service-DNS (Etap 3.1).
    //
    // Decouples "where a service lives right now" from "what other
    // stacks call it". Each row maps a logical name (`db`, `cache`,
    // `auth`) to the server that hosts it. The deploy pipeline injects
    // an `extra_hosts` entry per service_dns row alongside the
    // `<server>.mesh` entries already injected by Etap 0.3f, so a
    // consumer stack with `DATABASE_URL=postgres://db.mesh:5432/x`
    // keeps working after the operator moves postgres between nodes —
    // they just update this table and the dependent stacks get
    // re-injected hosts on next redeploy.
    //
    // Constraints worth knowing:
    // - `name` is the LEAF of the future `<name>.mesh` hostname (we
    //   add the `.mesh` suffix at injection time). Lowercase
    //   alphanumeric + hyphen, validated at the API layer.
    // - PRIMARY KEY(name) — one server per name in v1. Multi-replica
    //   load balancing is on the FUTURE list.
    // - server_id FK with ON DELETE CASCADE — if the host server is
    //   removed, the mapping disappears too; the next sync removes
    //   the stale extra_host on its own.
    // - service_id is optional so an operator can register a name
    //   that points at a port not yet associated with a managed
    //   service (e.g. an external postgres listening on the host).
    r#"
    CREATE TABLE IF NOT EXISTS service_dns (
        name        TEXT PRIMARY KEY NOT NULL,
        server_id   TEXT NOT NULL REFERENCES servers(id) ON DELETE CASCADE,
        service_id  TEXT,
        port        INTEGER NOT NULL,
        created_at  INTEGER NOT NULL,
        updated_at  INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_service_dns_server_id
        ON service_dns(server_id);
    "#,
    // Migration 55: Re-backfill global_role for installs whose first user was
    // created via /setup after migration 44 ran.
    //
    // Migration 44 added the `global_role` column with DEFAULT 'user' and
    // backfilled existing rows, but the setup handler kept INSERTing only the
    // legacy `role` column, so the installer ended up with role='admin' and
    // global_role='user'. They show as Admin on the profile screen but fail
    // every require_global_admin gate (proxy/update, team management).
    //
    // The setup handler is fixed alongside this migration to set global_role
    // explicitly; this migration repairs already-affected installs. Idempotent:
    // both UPDATEs no-op on healthy rows.
    r#"
    UPDATE users SET global_role = 'owner'
        WHERE global_role = 'user'
          AND role = 'admin'
          AND NOT EXISTS (SELECT 1 FROM users WHERE global_role = 'owner')
          AND id = (
              SELECT id FROM users WHERE role = 'admin'
              ORDER BY created_at ASC LIMIT 1
          );

    UPDATE users SET global_role = 'admin'
        WHERE role = 'admin' AND global_role = 'user';
    "#,
    // Migration 56: Stateless service migration lock (Etap 4.1).
    //
    // The migration orchestrator (POST /api/v1/stacks/{id}/migrate) is
    // a multi-step pipeline: snapshot definition → create on target →
    // wait for health → cut over domain → tear down source. Two
    // operators clicking Migrate at the same moment for the same
    // stack would race in step 3, both ending up with the same stack
    // running on both target nodes and traffic split.
    //
    // We protect against that with a row-level flag rather than an
    // in-process Mutex so concurrent operators on the same source node
    // (different browser tabs, API clients) all see the same answer.
    // The flag is cleared on success or rolled back on failure inside
    // the orchestrator's own state machine.
    r#"
    ALTER TABLE services ADD COLUMN migration_in_progress INTEGER NOT NULL DEFAULT 0;
    "#,
    // Migration 57: store upstream packument as a single JSON blob.
    //
    // The proxy mode used to fan an upstream packument into npm_versions —
    // one row per historical version. For popular packages (next: 3769,
    // @playwright/test: 3196, react: 2804) this bloated SQLite with rows
    // whose only useful field was manifest_json. We now keep the raw
    // upstream packument in `npm_packages.upstream_packument_json` and only
    // create `npm_versions` rows when a tarball is actually downloaded.
    //
    // The DELETE drops the metadata-only rows left over from the old
    // approach; downloaded versions (`tarball_size > 0`) survive and the
    // next packument fetch from upstream repopulates the blob.
    r#"
    ALTER TABLE npm_packages ADD COLUMN upstream_packument_json TEXT;
    DELETE FROM npm_versions
     WHERE tarball_size = 0
       AND package_name IN (SELECT name FROM npm_packages WHERE is_proxy = 1);
    "#,
    // Migration 58: pin flag for the proxy Mirror tab.
    //
    // When a Pier mirror caches a public package (say `next`), npm fans out
    // and ALSO caches every transitive dep (sharp, @next/swc-*, react,
    // scheduler, …). For an operator who only cares about packages they
    // explicitly installed, the Mirror list becomes noisy fast. `pinned = 1`
    // marks a package as primary-interest; the UI exposes a star toggle on
    // each row + a "Pinned only" filter on the listing.
    r#"
    ALTER TABLE npm_packages ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;
    CREATE INDEX IF NOT EXISTS idx_npm_packages_pinned ON npm_packages(pinned);
    "#,
    // Migration 59: Backfill proxy.acme_email from owner email on legacy installs.
    //
    // The /setup handler started seeding settings.proxy.acme_email from the
    // first admin email, but installs older than that commit have no row in
    // `settings` for this key. The UI reads the raw value and shows an empty
    // field with the "Not loaded — save to apply" warning, while the runtime
    // falls back to the owner email via proxy::read_acme_email() — so LE
    // actually issues certs but the operator sees a misconfiguration.
    //
    // We pick global_role='owner' (set by migrations 44/55) for determinism
    // on multi-admin installs. The SELECT yields zero rows on a fresh DB
    // before /setup ran, so the migration is a no-op then.
    //
    // The WHERE clause only fires when the existing value is missing or an
    // empty string — an operator-chosen non-empty value is preserved.
    r#"
    INSERT OR REPLACE INTO settings (key, value, updated_at)
    SELECT 'proxy.acme_email', u.email, datetime('now')
      FROM users u
     WHERE u.global_role = 'owner'
       AND u.email IS NOT NULL
       AND u.email != ''
       AND (
           NOT EXISTS (SELECT 1 FROM settings WHERE key = 'proxy.acme_email')
           OR (SELECT value FROM settings WHERE key = 'proxy.acme_email') = ''
       )
     LIMIT 1;
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
