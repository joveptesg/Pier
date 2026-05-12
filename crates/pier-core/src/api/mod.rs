pub mod account;
pub mod alerts;
pub mod auth;
pub mod backups;
pub mod canvas;
pub mod catalog;
pub mod compose;
pub mod containers;
pub mod databases;
pub mod deployments;
pub mod domains;
pub mod env;
pub mod events;
pub mod grants;
pub mod images;
pub mod networks;
pub mod npm;
pub mod projects;
pub mod promote;
pub mod proxy;
pub mod registries;
pub mod registry_settings;
pub mod resources;
pub mod s3;
pub mod servers;
pub mod sources;
pub mod system;
pub mod system_logs;
pub mod tokens;
pub mod webhooks;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{any, delete, get, patch, post, put};
use axum::Router;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::GovernorLayer;

use crate::auth::middleware::require_auth;
use crate::state::SharedState;

/// Health check endpoint — no auth required.
async fn health(State(state): State<SharedState>) -> axum::Json<serde_json::Value> {
    let docker_ok = state.docker.ping().await.is_ok();
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "docker": docker_ok,
    }))
}

/// Build the API router at /api/v1/*.
///
/// `tower_governor` rate-limits the unauthenticated auth endpoints by peer IP.
/// `per_second(n)` means "replenish one token every n seconds", so a 5-burst
/// with `per_second(12)` allows a 5-attempt burst and then throttles to roughly
/// one attempt per 12 seconds. Setup is more permissive on rate but a single
/// successful call locks the endpoint via the atomic insert in
/// [`auth::setup`].
pub fn api_router(state: SharedState) -> Router<SharedState> {
    let login_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(12)
            .burst_size(5)
            .finish()
            .expect("login governor config"),
    );
    let setup_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(6)
            .burst_size(10)
            .finish()
            .expect("setup governor config"),
    );

    let public = Router::new()
        .route(
            "/auth/login",
            post(auth::login).layer(GovernorLayer::new(login_governor)),
        )
        .route(
            "/auth/setup",
            post(auth::setup).layer(GovernorLayer::new(setup_governor)),
        )
        // Health check
        .route("/health", get(health))
        // Agent heartbeat (token-based auth, not session)
        .route("/servers/heartbeat", post(servers::heartbeat))
        // Webhooks (public — GitHub/GitLab need to reach these)
        .route("/webhooks/github", post(webhooks::github))
        .route("/webhooks/gitlab", post(webhooks::gitlab))
        // GitHub App manifest callback (public — GitHub redirects here)
        .route("/sources/github/callback", get(sources::github_callback));

    let protected = Router::new()
        // Auth
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session_check))
        // Account (current-user profile, password, sessions)
        .route("/account/me", get(account::me))
        .route("/account/password", put(account::change_password))
        .route("/account/sessions", get(account::list_sessions))
        .route(
            "/account/sessions/revoke-others",
            post(account::revoke_other_sessions),
        )
        .route("/account/sessions/{id}", delete(account::revoke_session))
        // Bearer API tokens (used by npm registry, CI, CLI integrations)
        .route("/account/tokens", get(tokens::list).post(tokens::create))
        .route("/account/tokens/{id}", delete(tokens::revoke))
        // Embedded npm registry settings (which S3 storage to mirror to)
        .route(
            "/registry/settings",
            get(registry_settings::get).put(registry_settings::update),
        )
        // Containers
        .route("/containers", get(containers::list))
        .route(
            "/containers/{id}",
            get(containers::inspect).delete(containers::remove),
        )
        .route("/containers/{id}/start", post(containers::start))
        .route("/containers/{id}/stop", post(containers::stop))
        .route("/containers/{id}/restart", post(containers::restart))
        .route("/containers/{id}/logs", get(containers::logs))
        .route("/containers/{id}/logs/ws", get(containers::logs_ws))
        .route("/containers/all-stats", get(containers::all_stats))
        .route("/containers/{id}/stats", get(containers::stats))
        // Docker events fan-out (live container lifecycle)
        .route("/events/ws", get(events::events_ws))
        // Images
        .route("/images", get(images::list))
        .route("/images/{id}", delete(images::remove))
        // Stacks
        .route("/stacks", get(compose::list).post(compose::create))
        .route(
            "/stacks/{id}",
            get(compose::get)
                .put(compose::update)
                .delete(compose::remove),
        )
        .route("/stacks/{id}/deploy", post(compose::deploy))
        .route("/stacks/{id}/down", post(compose::down))
        // Projects
        .route("/projects", get(projects::list).post(projects::create))
        .route(
            "/projects/{id}",
            get(projects::get)
                .put(projects::update)
                .delete(projects::delete),
        )
        // Registry credentials (per-project + global)
        .route(
            "/registries",
            get(registries::list).post(registries::create),
        )
        .route(
            "/registries/{id}",
            put(registries::update).delete(registries::remove),
        )
        .route("/registries/{id}/test", post(registries::test))
        // Catalog
        .route("/catalog", get(catalog::list))
        .route("/catalog/{id}", get(catalog::get))
        // Resources
        .route("/resources", get(resources::list).post(resources::create))
        .route(
            "/resources/{id}",
            get(resources::get).delete(resources::remove),
        )
        .route("/resources/{id}/start", post(resources::start))
        .route("/resources/{id}/stop", post(resources::stop))
        .route("/resources/{id}/restart", post(resources::restart))
        .route("/resources/{id}/redeploy", post(resources::redeploy))
        .route("/resources/{id}/nodes", get(resources::get_nodes))
        .route("/resources/{id}/scale", post(resources::scale))
        .route(
            "/resources/{id}/load-balance",
            get(resources::get_load_balance).post(resources::load_balance),
        )
        .route(
            "/resources/{id}/port-public",
            put(resources::set_port_public),
        )
        .route("/resources/{id}/network", put(resources::set_network))
        .route("/resources/{id}/settings", put(resources::update_settings))
        .route("/resources/{id}/rename", put(resources::rename))
        .route(
            "/resources/{id}/deployment-logs",
            get(resources::deployment_logs),
        )
        // Git config
        .route(
            "/resources/{id}/git",
            get(resources::get_git_config).put(resources::update_git_config),
        )
        .route(
            "/resources/{id}/git-compose",
            get(resources::get_git_compose),
        )
        .route(
            "/resources/{id}/reload-compose",
            post(resources::reload_compose),
        )
        .route(
            "/resources/{id}/compose-services",
            get(resources::get_compose_services),
        )
        // Deployments (CI/CD pipeline)
        .route("/resources/{id}/deploy", post(deployments::manual_deploy))
        .route("/resources/{id}/rollback", post(deployments::rollback))
        .route("/resources/{id}/deployments", get(deployments::list))
        .route(
            "/resources/{id}/deployments/{dep_id}",
            get(deployments::get),
        )
        .route(
            "/resources/{id}/deployments/{dep_id}/cancel",
            post(deployments::cancel),
        )
        // Database management (PostgreSQL/MySQL)
        .route(
            "/resources/{id}/databases",
            get(databases::list_databases).post(databases::create_database),
        )
        .route(
            "/resources/{id}/databases/{dbname}",
            delete(databases::delete_database),
        )
        .route(
            "/resources/{id}/databases/{dbname}/password",
            put(databases::change_password),
        )
        // Restore a per-DB backup directly from a user-uploaded file.
        // Body limit raised to 5 GB to accommodate full Postgres dumps;
        // applied per-route so other endpoints keep their default 2 MB cap.
        .route(
            "/resources/{id}/databases/{dbname}/restore-upload",
            post(backups::restore_database_from_upload)
                .layer(DefaultBodyLimit::max(5 * 1024 * 1024 * 1024)),
        )
        // Environment Variables
        .route(
            "/resources/{id}/env",
            get(env::get_env).put(env::update_env),
        )
        // Backups
        .route("/resources/{id}/backups", get(backups::list_backups))
        .route(
            "/resources/{id}/backup-schedules",
            get(backups::list_schedules).post(backups::create_schedule),
        )
        .route(
            "/resources/{id}/backup-schedules/{schedule_id}",
            patch(backups::update_schedule).delete(backups::delete_schedule),
        )
        .route(
            "/resources/{id}/backups/trigger",
            post(backups::trigger_backup),
        )
        .route("/backups/{backup_id}", delete(backups::delete_backup))
        .route(
            "/backups/{backup_id}/download",
            get(backups::download_backup),
        )
        .route(
            "/backups/{backup_id}/restore",
            post(backups::restore_backup),
        )
        // Sources
        .route("/sources", get(sources::list).post(sources::create))
        .route("/sources/{id}", get(sources::get).delete(sources::remove))
        .route("/sources/{id}/repos", get(sources::list_repos))
        .route(
            "/sources/{id}/branches/{*repo}",
            get(sources::list_branches),
        )
        .route("/sources/{id}/file", get(sources::get_file))
        .route("/sources/github/manifest", get(sources::github_manifest))
        // S3 Storages
        .route("/s3", get(s3::list).post(s3::create))
        .route("/s3/{id}", put(s3::update).delete(s3::remove))
        .route("/s3/{id}/test", post(s3::test))
        // Servers
        .route("/servers", get(servers::list).post(servers::create))
        .route("/servers/install-script", get(servers::install_script))
        .route("/servers/{id}", get(servers::get).delete(servers::remove))
        .route("/servers/{id}/name", put(servers::rename))
        .route("/servers/{id}/test", post(servers::test_connection))
        .route("/servers/{id}/metrics", get(servers::metrics))
        .route("/servers/{id}/containers", get(servers::containers))
        .route("/servers/{id}/deploy", post(servers::deploy_to_server))
        .route("/servers/{id}/stop", post(servers::stop_on_server))
        // Proxy API calls to a kind='peer' server (Core↔Core federation).
        .route("/servers/{id}/proxy/{*rest}", any(servers::proxy))
        // Mode 3 — export or apply a promotion bundle so this server can graduate to a standalone pier-core.
        .route("/servers/{id}/promote-bundle", get(promote::bundle))
        .route("/servers/{id}/promote", post(promote::trigger))
        // Federation handshake: remote peer-cores probe here with their grant token.
        .route("/peers/probe", get(grants::probe))
        // External access — tokens that authorize another pier-core to control this one.
        .route("/grants", get(grants::list).post(grants::create))
        .route("/grants/{id}", delete(grants::revoke))
        // Canvas (architect view)
        .route("/canvas", get(canvas::get_canvas))
        .route("/canvas/positions", put(canvas::save_positions))
        // Networks
        .route("/networks", get(networks::list).post(networks::create))
        .route("/networks/{id}", delete(networks::delete))
        // Domains
        .route("/domains", get(domains::list).post(domains::create))
        .route("/domains/{id}", delete(domains::remove))
        .route("/resources/{id}/domains", get(domains::list_for_service))
        // Proxy
        .route("/proxy/enable", post(proxy::enable))
        .route("/proxy/disable", post(proxy::disable))
        .route("/proxy/status", get(proxy::status))
        .route("/proxy/settings", put(proxy::update_settings))
        .route("/proxy/version", get(proxy::version))
        .route("/proxy/update", post(proxy::update))
        // System
        .route("/system/metrics", get(system::metrics))
        .route("/system/docker", get(system::docker_info))
        .route("/system/info", get(system::info))
        .route("/system/disk-usage", get(system::disk_usage))
        .route("/system/cleanup-info", get(system::cleanup_info))
        .route("/system/cleanup", post(system::cleanup))
        .route(
            "/system/cleanup-settings",
            get(system::cleanup_settings_get).put(system::cleanup_settings_update),
        )
        .route("/system/update-check", get(system::update_check))
        .route("/system/update", post(system::update_now))
        .route(
            "/system/update-settings",
            get(system::update_settings).put(system::save_update_settings),
        )
        .route(
            "/system/timezone",
            get(system::get_timezone).put(system::set_timezone),
        )
        // System logs (journalctl) — pier / pier-agent units only
        .route("/system/logs", get(system_logs::snapshot))
        .route("/system/logs/units", get(system_logs::units_list))
        .route("/system/logs/ws", get(system_logs::stream_ws))
        // Alerts (Phase 11.5) — advanced/custom rules
        .route("/alerts", get(alerts::list).post(alerts::create))
        .route("/alerts/events", get(alerts::events_feed))
        .route(
            "/alerts/{id}",
            get(alerts::get).put(alerts::update).delete(alerts::remove),
        )
        .route("/alerts/{id}/toggle", post(alerts::toggle))
        .route("/alerts/{id}/test", post(alerts::test))
        .route("/alerts/{id}/events", get(alerts::rule_events))
        // Notifications — simplified UI layer (global channel + preset toggles)
        .route(
            "/notifications/channels/telegram",
            get(alerts::channel_get).put(alerts::channel_put),
        )
        .route(
            "/notifications/channels/telegram/test",
            post(alerts::channel_test),
        )
        .route(
            "/notifications/channels/email",
            get(alerts::channel_email_get).put(alerts::channel_email_put),
        )
        .route(
            "/notifications/channels/email/test",
            post(alerts::channel_email_test),
        )
        .route(
            "/notifications/channels/discord",
            get(alerts::channel_discord_get).put(alerts::channel_discord_put),
        )
        .route(
            "/notifications/channels/discord/test",
            post(alerts::channel_discord_test),
        )
        .route(
            "/notifications/channels/slack",
            get(alerts::channel_slack_get).put(alerts::channel_slack_put),
        )
        .route(
            "/notifications/channels/slack/test",
            post(alerts::channel_slack_test),
        )
        .route("/notifications/alerts", get(alerts::preset_list))
        .route("/notifications/alerts/{id}/toggle", post(alerts::toggle))
        .layer(axum::middleware::from_fn_with_state(state, require_auth));

    Router::new()
        // Health at root level for easy monitoring
        .route("/health", get(health))
        .nest("/api/v1", public.merge(protected))
}
