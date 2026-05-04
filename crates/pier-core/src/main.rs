mod alerts;
mod api;
mod auth;
mod backup;
mod catalog;
mod config;
pub mod crypto;
mod crypto_recovery;
mod db;
mod deploy;
mod docker;
mod error;
mod git;
mod proxy;
mod registry;
mod s3;
mod state;
mod timezone;
mod ui;

use std::sync::{Arc, Mutex};

use anyhow::Result;
use bollard::Docker;
use tracing_subscriber::EnvFilter;

use config::PierConfig;
use state::AppState;

#[tokio::main]
async fn main() -> Result<()> {
    // One-shot CLI modes handled before the long-lived server starts.
    // Usage: `pier --import-bundle path/to/bundle.json` — hydrate a fresh DB
    // from a bundle exported by another core's `/api/v1/servers/{id}/promote-bundle`
    // endpoint, then exit.
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "--import-bundle" {
        return import_bundle_cli(&args[2]);
    }

    let config = PierConfig::from_env();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .init();

    tracing::info!("Pier v{}", env!("CARGO_PKG_VERSION"));

    // Crypto self-check: loads + caches PIER_SECRET and verifies encrypt/decrypt
    // roundtrip. Panics loudly if the key cannot be persisted or is broken —
    // we never want to run with a silently-wrong key that would corrupt user data.
    crypto::self_check();
    tracing::info!("Crypto self-check OK");

    // Initialize database
    let conn = db::init_db(&config.db_path)?;

    // SEC-007: restrict database file permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&config.db_path, std::fs::Permissions::from_mode(0o600));
    }

    // Create pre-encryption backup of pier.db (one-time, before any encryption happens)
    {
        let backup_path = config.data_dir.join("pier.db.pre-encryption");
        if !backup_path.exists() {
            if let Err(e) = std::fs::copy(&config.db_path, &backup_path) {
                tracing::warn!("Could not create pre-encryption DB backup: {e}");
            } else {
                tracing::info!("Created pre-encryption backup: {}", backup_path.display());
            }
        }
    }

    // Daily local backup of pier.db + .env
    {
        let data_dir = config.data_dir.clone();
        let db_path = config.db_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
                let backup_dir = data_dir.join("backups").join("system");
                let _ = tokio::fs::create_dir_all(&backup_dir).await;
                let ts = chrono::Local::now().format("%Y%m%d").to_string();
                // Backup pier.db
                let db_backup = backup_dir.join(format!("pier-{ts}.db"));
                if let Err(e) = tokio::fs::copy(&db_path, &db_backup).await {
                    tracing::warn!("System backup pier.db failed: {e}");
                }
                // Backup .env
                for env_path in &["/opt/pier/.env", ".env"] {
                    let p = std::path::Path::new(env_path);
                    if p.exists() {
                        let env_backup = backup_dir.join(format!("env-{ts}.bak"));
                        let _ = tokio::fs::copy(p, &env_backup).await;
                        break;
                    }
                }
                // Keep only last 7 backups
                if let Ok(mut entries) = tokio::fs::read_dir(&backup_dir).await {
                    let mut files: Vec<std::path::PathBuf> = Vec::new();
                    while let Ok(Some(entry)) = entries.next_entry().await {
                        if entry.path().extension().map(|e| e == "db").unwrap_or(false) {
                            files.push(entry.path());
                        }
                    }
                    files.sort();
                    while files.len() > 7 {
                        if let Some(old) = files.first() {
                            let _ = tokio::fs::remove_file(old).await;
                            // Also remove matching env backup
                            let env_name = old
                                .file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .replace("pier-", "env-")
                                .replace(".db", ".bak");
                            let _ = tokio::fs::remove_file(backup_dir.join(env_name)).await;
                        }
                        files.remove(0);
                    }
                }
                tracing::info!("System backup completed: pier.db + .env");
            }
        });
    }

    // Check if setup is needed
    let user_count = db::user_count(&conn)?;
    if user_count == 0 {
        tracing::info!("No users found — visit /setup to create admin account");
    } else {
        tracing::info!("{user_count} user(s) in database");
    }

    // Connect to Docker
    let docker = match &config.docker_host {
        Some(host) => Docker::connect_with_http(host, 120, bollard::API_DEFAULT_VERSION)?,
        None => Docker::connect_with_local_defaults()?,
    };

    // Verify Docker connection
    let docker_ok = match docker.ping().await {
        Ok(_) => {
            tracing::info!("Docker connection OK");
            true
        }
        Err(e) => {
            tracing::warn!("Docker not available: {e} — container features will fail");
            false
        }
    };

    // Read proxy settings before conn moves into Mutex.
    // `proxy.acme_email` is intentionally NOT read here — we resolve it inside
    // the auto-start task via `proxy::read_acme_email`, so a `/setup` that runs
    // after Pier already started still propagates without a Pier restart.
    let proxy_dashboard = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.dashboard'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "false".to_string())
        == "true";
    let proxy_platform_domain = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.platform_domain'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    let proxy_traefik_version = conn
        .query_row(
            "SELECT value FROM settings WHERE key = 'proxy.traefik_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|v: &String| !v.is_empty())
        .unwrap_or_else(|| proxy::DEFAULT_TRAEFIK_VERSION.to_string());

    // Initialize templates
    let templates = ui::templates::init_templates();

    // Load service catalog
    let catalog = catalog::load_catalog();
    tracing::info!("Loaded {} catalog templates", catalog.len());

    // Spawn the Docker events fan-out before state is built so handlers can
    // subscribe from moment one (even before Traefik auto-deploy completes).
    let event_bus = docker::events::DockerEventBus::spawn(docker.clone());
    tracing::info!("Docker events bus started");

    // Build shared state
    let state = Arc::new(AppState {
        db: Mutex::new(conn),
        docker,
        event_bus,
        templates,
        config: config.clone(),
        catalog,
        started_at: std::time::Instant::now(),
        ssl_notify: Arc::new(tokio::sync::Notify::new()),
    });

    // One-shot recovery of env_json entries encrypted with historical random
    // keys (install.sh drops .pier-recovery-keys from journald). No-op if the
    // file is absent. Runs synchronously — fast (a few dozen rows).
    crypto_recovery::run_recovery_if_needed(&state);

    // Start backup scheduler
    backup::scheduler::start_scheduler(state.clone());
    tracing::info!("Backup scheduler started");

    // Start SSL certificate monitor (checks acme.json every 15 min)
    proxy::ssl_monitor::start_ssl_monitor(state.clone());
    tracing::info!("SSL monitor started");

    // Start alerts scheduler (checks metrics every 30s)
    alerts::start_scheduler(state.clone());
    tracing::info!("Alerts scheduler started");

    // Cleanup invalid domains (with https:// prefix) and their Traefik configs
    {
        if let Ok(db) = state.db.lock() {
            let mut stmt = db.prepare("SELECT id FROM domains WHERE domain LIKE 'https://%' OR domain LIKE 'http://%'").unwrap();
            let invalid_ids: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            for did in &invalid_ids {
                let _ = db.execute("DELETE FROM domains WHERE id = ?1", [did]);
                let config_path = state
                    .config
                    .data_dir
                    .join("traefik")
                    .join("dynamic")
                    .join(format!("{did}.yml"));
                let _ = std::fs::remove_file(&config_path);
            }
            if !invalid_ids.is_empty() {
                tracing::info!(
                    "Cleaned up {} invalid domain(s) with protocol prefix",
                    invalid_ids.len()
                );
            }
        }
    }

    // Auto-start proxy (Traefik) in background
    if docker_ok {
        let proxy_state = state.clone();
        let proxy_data_dir = config.data_dir.clone();
        let proxy_port = config.port;
        tokio::spawn(async move {
            // Auto-detect and cache public IP + geolocation
            let public_ip = match proxy::config::detect_public_ip().await {
                Ok(ip) => {
                    tracing::info!("Detected public IP: {ip}");
                    if let Ok(db) = proxy_state.db.lock() {
                        let _ = db.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('server.public_ip', ?1)",
                            [&ip],
                        );
                    }
                    Some(ip)
                }
                Err(e) => {
                    tracing::warn!("Could not detect public IP: {e}");
                    None
                }
            };

            // Ensure local server record exists + detect geolocation
            if let Some(ref ip) = public_ip {
                if let Ok(db) = proxy_state.db.lock() {
                    // Self-heal: fresh installs done after migration 31 was added
                    // ended up with kind='agent' on the local row because the INSERT
                    // below didn't set kind explicitly and the column's DEFAULT is 'agent'.
                    let _ = db.execute(
                        "UPDATE servers SET kind = 'local' WHERE is_local = 1 AND kind <> 'local'",
                        [],
                    );

                    // Deduplicate: keep only one local server
                    let local_count: i64 = db
                        .query_row(
                            "SELECT COUNT(*) FROM servers WHERE is_local = 1",
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(0);

                    if local_count > 1 {
                        // Keep the first one, delete the rest
                        let first_id: String = db
                            .query_row(
                                "SELECT id FROM servers WHERE is_local = 1 ORDER BY created_at ASC LIMIT 1",
                                [],
                                |row| row.get(0),
                            )
                            .unwrap_or_else(|_| "local".to_string());
                        let _ = db.execute(
                            "DELETE FROM servers WHERE is_local = 1 AND id != ?1",
                            [&first_id],
                        );
                        tracing::info!("Removed duplicate local server records");
                    }

                    if local_count == 0 {
                        let hostname =
                            sysinfo::System::host_name().unwrap_or_else(|| "localhost".to_string());
                        let _ = db.execute(
                            "INSERT INTO servers (id, name, host, port, agent_token, status, is_local, kind, os_info)
                             VALUES ('local', ?1, ?2, 0, '', 'online', 1, 'local', ?3)",
                            rusqlite::params![
                                hostname,
                                ip,
                                format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                            ],
                        );
                        tracing::info!("Created local server record: {hostname}");
                    } else {
                        // Update host IP and system info
                        let _ = db.execute(
                            "UPDATE servers SET host = ?1, status = 'online', os_info = ?2, updated_at = datetime('now') WHERE is_local = 1",
                            rusqlite::params![ip, format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)],
                        );
                    }
                }

                // Detect geolocation via ip-api.com (free, no key required)
                match detect_geolocation(ip).await {
                    Ok((country, city, code)) => {
                        tracing::info!("Server location: {city}, {country} ({code})");
                        if let Ok(db) = proxy_state.db.lock() {
                            let _ = db.execute(
                                "UPDATE servers SET country = ?1, city = ?2, country_code = ?3 WHERE is_local = 1",
                                rusqlite::params![country, city, code],
                            );
                        }
                    }
                    Err(e) => tracing::warn!("Geolocation detection failed: {e}"),
                }
            }

            // Resolve ACME email at auto-start time (not at process start) so a
            // `/setup` that completes after Pier boots still gives Let's Encrypt
            // a valid contact instead of the hardcoded `admin@pier.local`.
            let acme_email = proxy_state
                .db
                .lock()
                .ok()
                .map(|db| proxy::read_acme_email(&db))
                .unwrap_or_else(|| "admin@pier.local".to_string());

            // Deploy Traefik
            match proxy::deploy_traefik(
                &proxy_state.docker,
                &proxy_data_dir,
                &acme_email,
                proxy_dashboard,
                &proxy_traefik_version,
            )
            .await
            {
                Ok(_) => {
                    tracing::info!("Proxy auto-started (Traefik)");
                    if let Ok(db) = proxy_state.db.lock() {
                        let _ = db.execute(
                            "INSERT OR REPLACE INTO settings (key, value) VALUES ('proxy.enabled', 'true')",
                            [],
                        );
                    }
                    // Write platform domain config if set
                    if !proxy_platform_domain.is_empty() {
                        let target = format!("http://host.docker.internal:{proxy_port}");
                        if let Err(e) = proxy::config::write_platform_domain_config(
                            &proxy_data_dir,
                            &proxy_platform_domain,
                            &target,
                        ) {
                            tracing::warn!("Failed to write platform domain config: {e}");
                        }
                    }
                }
                Err(e) => tracing::warn!("Proxy auto-start failed: {e}"),
            }
        });
    }

    // Build router: UI + API
    // Scheduled cleanup: configurable Docker pruning
    {
        let cleanup_state = state.clone();
        tokio::spawn(async move {
            loop {
                // Read settings (default: enabled, 24h, prune images + build cache)
                let (enabled, interval_h, do_cache, do_images, do_containers) = {
                    let db = cleanup_state.db.lock().ok();
                    let get = |key: &str, default: &str| -> String {
                        db.as_ref()
                            .and_then(|db| {
                                db.query_row(
                                    "SELECT value FROM settings WHERE key = ?1",
                                    [key],
                                    |row| row.get(0),
                                )
                                .ok()
                            })
                            .unwrap_or_else(|| default.to_string())
                    };
                    (
                        get("cleanup.enabled", "true") == "true",
                        get("cleanup.interval_hours", "24")
                            .parse::<u64>()
                            .unwrap_or(24),
                        get("cleanup.prune_build_cache", "true") != "false",
                        get("cleanup.prune_images", "true") != "false",
                        get("cleanup.prune_containers", "false") == "true",
                    )
                };

                tokio::time::sleep(std::time::Duration::from_secs(interval_h * 3600)).await;

                if !enabled {
                    continue;
                }

                let state_ref = cleanup_state.clone();
                let run = |name: &'static str, args: &[&str]| {
                    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
                    let s = state_ref.clone();
                    async move {
                        match tokio::process::Command::new("docker")
                            .args(&args)
                            .output()
                            .await
                        {
                            Ok(out) => {
                                let stdout =
                                    String::from_utf8_lossy(&out.stdout).trim().to_string();
                                tracing::info!("Cleanup {name}: {stdout}");
                                alerts::hooks::fire_event(
                                    &s,
                                    "docker_cleanup_success",
                                    None,
                                    format!("Docker {name} pruned: {stdout}"),
                                )
                                .await;
                            }
                            Err(e) => {
                                tracing::warn!("Cleanup {name} failed: {e}");
                                alerts::hooks::fire_event(
                                    &s,
                                    "docker_cleanup_failure",
                                    None,
                                    format!("Docker {name} prune failed: {e}"),
                                )
                                .await;
                            }
                        }
                    }
                };

                if do_images {
                    run("images", &["image", "prune", "-f"]).await;
                }
                if do_cache {
                    run("build_cache", &["builder", "prune", "-f"]).await;
                }
                if do_containers {
                    run("containers", &["container", "prune", "-f"]).await;
                }
            }
        });
    }

    // Peer-core heartbeat: refresh status for every registered federated core.
    api::servers::spawn_heartbeat_task(state.clone());

    // Drop orphan registry tarballs from a publish that crashed between
    // FS write and DB insert. Best-effort; never fails startup.
    {
        let s = state.clone();
        tokio::spawn(async move {
            if let Err(e) = registry::storage::gc_orphans(&s).await {
                tracing::warn!("registry: orphan gc skipped: {e:#}");
            }
        });
    }

    let app = ui::ui_router(state.clone())
        .merge(api::api_router(state.clone()))
        // Embedded npm-compatible registry. Lives outside `/api/v1/` because
        // npm clients expect `<registry-url>/{package}` to be the canonical
        // packument path — they don't accept extra prefixes.
        .nest("/registry/npm", api::npm::router(state.clone()))
        .with_state(state);

    // Start server
    let addr = config.listen_addr();
    tracing::info!("Listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

/// Hydrate a fresh pier-core DB from a bundle file. Invoked via `--import-bundle`.
/// Exits the process on completion — the server does not start afterward, so the
/// operator can inspect the import result before running pier normally.
fn import_bundle_cli(path: &str) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = PierConfig::from_env();
    tracing::info!(
        "Importing bundle from {path} into {}",
        config.db_path.display()
    );

    crypto::self_check();

    let raw =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("reading bundle {path}: {e}"))?;
    let bundle: api::promote::PromoteBundle =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("parsing bundle: {e}"))?;

    tracing::info!(
        "Bundle source: server '{}' (id {}), exported {}, pier v{}",
        bundle.source_server_name,
        bundle.source_server_id,
        bundle.exported_at,
        bundle.pier_version
    );

    let conn = db::init_db(&config.db_path)?;
    let summary = api::promote::import_bundle(&conn, &bundle)?;
    tracing::info!(
        "Import complete: {} rows across {} tables",
        summary.total_rows,
        summary.per_table.len()
    );
    for (table, count) in &summary.per_table {
        tracing::info!("  {table}: {count}");
    }
    Ok(())
}

/// Detect server geolocation from public IP using ip-api.com (free, no API key).
async fn detect_geolocation(ip: &str) -> anyhow::Result<(String, String, String)> {
    let url = format!("http://ip-api.com/json/{ip}?fields=country,city,countryCode");
    let resp: serde_json::Value = reqwest::get(&url).await?.json().await?;
    let country = resp["country"].as_str().unwrap_or("Unknown").to_string();
    let city = resp["city"].as_str().unwrap_or("Unknown").to_string();
    let code = resp["countryCode"].as_str().unwrap_or("XX").to_string();
    Ok((country, city, code))
}
