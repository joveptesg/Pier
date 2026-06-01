pub mod account;
pub mod alerts;
pub mod audit;
pub mod auth;
pub mod backups;
pub mod canvas;
pub mod catalog;
pub mod compose;
pub mod containers;
pub mod databases;
#[cfg(feature = "db-browser")]
pub mod db_browser;
#[cfg(feature = "db-browser")]
pub mod db_nosql;
pub mod deployments;
pub mod domains;
pub mod env;
pub mod events;
pub mod federation;
pub mod federation_agent;
pub mod federation_tokens;
pub mod grants;
pub mod images;
pub mod install;
pub mod invitations;
pub mod migration;
// Note: `networks` (plural) is the Docker-networks management API.
// `network` (singular) below is the host-level WireGuard mesh.
pub mod network;
pub mod networks;
pub mod npm;
pub mod npm_web_login;
pub mod project_members;
pub mod projects;
pub mod promote;
pub mod proxy;
pub mod registries;
pub mod registry_admin;
pub mod registry_settings;
pub mod resources;
pub mod s3;
pub mod schedules;
pub mod security;
pub mod servers;
pub mod service_dns;
pub mod settings_railpack;
pub mod sources;
pub mod system;
pub mod system_logs;
pub mod tasks;
pub mod tokens;
pub mod users;
pub mod webhooks;

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::{any, delete, get, patch, post, put};
use axum::Router;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::GovernorLayer;

use crate::auth::middleware::require_auth;
use crate::auth::rbac::guards::{require_global_admin, require_global_owner};
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
    // Same throttle profile as /auth/login — without it a stolen partial token
    // gives an attacker a ~5-minute window to brute-force a 6-digit TOTP code.
    let login_2fa_governor = Arc::new(
        GovernorConfigBuilder::default()
            .per_second(12)
            .burst_size(5)
            .finish()
            .expect("login_2fa governor config"),
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
            post(auth::login).layer(GovernorLayer::new(login_governor.clone())),
        )
        .route(
            "/auth/login/2fa",
            post(auth::login_2fa).layer(GovernorLayer::new(login_2fa_governor)),
        )
        .route(
            "/auth/setup",
            post(auth::setup).layer(GovernorLayer::new(setup_governor)),
        )
        // Health check
        .route("/health", get(health))
        // Agent heartbeat (token-based auth, not session)
        .route("/servers/heartbeat", post(servers::heartbeat))
        // Agent handshake — exchanges a one-shot bootstrap token for the
        // long-term agent_token. Public because install.sh calls it before
        // the agent has any session/long-term credential.
        .route("/servers/{id}/handshake", post(servers::handshake))
        // Webhooks (public — GitHub/GitLab need to reach these)
        .route("/webhooks/github", post(webhooks::github))
        .route("/webhooks/gitlab", post(webhooks::gitlab))
        // GitHub App manifest callback (public — GitHub redirects here)
        .route("/sources/github/callback", get(sources::github_callback))
        // Invitation accept — recipient is anonymous until they POST.
        // Token is verified in-handler via sha256 lookup; rate-limited
        // by reusing the same governor profile as login to slow guessing.
        .route(
            "/invitations/{token}",
            get(invitations::get).layer(GovernorLayer::new(login_governor.clone())),
        )
        .route(
            "/invitations/{token}/accept",
            post(invitations::accept).layer(GovernorLayer::new(login_governor.clone())),
        );

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
        // `npm login --auth-type=web` — panel side of the flow.
        // CLI-side endpoints live under `/registry/npm/-/v1/...`.
        .merge(npm_web_login::protected_router())
        // 2FA (TOTP) enrollment / disable
        .route("/account/2fa", get(account::two_fa_status))
        .route("/account/2fa/setup", post(account::two_fa_setup))
        .route("/account/2fa/verify", post(account::two_fa_verify))
        .route("/account/2fa/disable", post(account::two_fa_disable))
        // Note: /audit/events moved to admin_only — previously accessible to
        // any authenticated user, which leaked the auth event log.
        // Project membership (project-scoped RBAC enforced in-handler).
        // Listed first so they appear next to `/projects` for navigability.
        .route(
            "/projects/{id}/members",
            get(project_members::list).post(project_members::add),
        )
        .route(
            "/projects/{id}/members/{user_id}",
            put(project_members::update_role).delete(project_members::remove),
        )
        // Security settings (delete confirmation, etc.) — self-owned.
        .route(
            "/security/settings",
            get(security::get_settings).put(security::update_settings),
        )
        // Projects — list filters by membership, get/update/delete gated
        // per-project in the handler. Create is Admin-only via inline check.
        .route("/projects", get(projects::list).post(projects::create))
        .route(
            "/projects/{id}",
            get(projects::get)
                .put(projects::update)
                .delete(projects::delete),
        )
        // Catalog — read-only template browsing for any authenticated user.
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
        // Servers — read-only endpoints stay here so any authenticated user
        // can see infrastructure (needed for resource lists, project context).
        // Mutations (POST /servers, DELETE, /name, /test, /rotate, /deploy,
        // /stop, /promote*) live in `admin_only`.
        .route("/servers", get(servers::list))
        .route("/servers/{id}", get(servers::get))
        .route("/servers/{id}/metrics", get(servers::metrics))
        .route("/servers/{id}/containers", get(servers::containers))
        // Federation proxy — peer-token requests route through here too.
        // The handler enforces its own per-call policy.
        .route("/servers/{id}/proxy/{*rest}", any(servers::proxy))
        // Federation peer probe — anonymous peers hit this with their
        // X-Pier-Peer-Token; require_auth recognises the header and the
        // handler returns identity info. Not admin-gated.
        .route("/peers/probe", get(grants::probe))
        // Read-only federation cache view.
        .route("/federation/projects", get(federation::list_projects))
        .route("/federation/stacks", get(federation::list_stacks))
        .route("/federation/status", get(federation::status))
        // Canvas (architect view) — User+ since it's a project-bound visual.
        .route("/canvas", get(canvas::get_canvas))
        .route("/canvas/positions", put(canvas::save_positions))
        // Domains — list/create/delete gated in-handler via the service's
        // project membership.
        .route("/domains", get(domains::list).post(domains::create))
        .route(
            "/domains/{id}",
            put(domains::update).delete(domains::remove),
        )
        .route("/domains/{id}/activate", post(domains::activate))
        .route("/domains/{id}/deactivate", post(domains::deactivate))
        .route("/resources/{id}/domains", get(domains::list_for_service))
        // Proxy read-only — anyone can see status. Mutating ops moved to
        // admin_only.
        .route("/proxy/status", get(proxy::status))
        .route("/proxy/version", get(proxy::version))
        // System read-only — metrics + info available to all authenticated
        // users; mutating ops (cleanup, update, timezone, logs) live in
        // admin_only.
        .route("/system/metrics", get(system::metrics))
        .route("/system/docker", get(system::docker_info))
        .route("/system/info", get(system::info))
        .route("/system/disk-usage", get(system::disk_usage))
        .route("/system/cleanup-info", get(system::cleanup_info))
        .route("/system/update-check", get(system::update_check));

    // In-panel database browser + SQL runner. Browser reads are Viewer+; the
    // runner is Editor+ (each handler self-gates via `enforce_resource_role`).
    // Compiled only with the `db-browser` feature so size-sensitive builds drop
    // sqlx entirely.
    #[cfg(feature = "db-browser")]
    let protected = protected
        .route(
            "/resources/{id}/db-browser/databases",
            get(db_browser::list_databases),
        )
        .route(
            "/resources/{id}/db-browser/objects",
            get(db_browser::objects),
        )
        .route(
            "/resources/{id}/db-browser/structure",
            get(db_browser::structure),
        )
        .route("/resources/{id}/db-browser/rows", get(db_browser::rows))
        .route(
            "/resources/{id}/db-browser/query",
            post(db_browser::run_query),
        )
        // Redis / Valkey (native client)
        .route(
            "/resources/{id}/db-browser/redis/keyspace",
            get(db_nosql::redis_keyspace),
        )
        .route(
            "/resources/{id}/db-browser/redis/keys",
            get(db_nosql::redis_keys),
        )
        .route(
            "/resources/{id}/db-browser/redis/value",
            get(db_nosql::redis_value),
        )
        .route(
            "/resources/{id}/db-browser/redis/command",
            post(db_nosql::redis_command),
        )
        .route(
            "/resources/{id}/db-browser/redis/monitor/ws",
            get(db_nosql::redis_monitor_ws),
        )
        // MongoDB (mongosh via docker-exec)
        .route(
            "/resources/{id}/db-browser/mongo/databases",
            get(db_nosql::mongo_databases),
        )
        .route(
            "/resources/{id}/db-browser/mongo/collections",
            get(db_nosql::mongo_collections),
        )
        .route(
            "/resources/{id}/db-browser/mongo/documents",
            get(db_nosql::mongo_documents),
        )
        .route(
            "/resources/{id}/db-browser/mongo/query",
            post(db_nosql::mongo_query),
        );

    // Global-Admin sub-router. Anything here is reached by:
    //   1. require_auth populates AuthUser in extensions
    //   2. require_global_admin checks `global_role >= Admin`
    // The `route_layer` form applies the guard *only* to existing routes
    // (vs `.layer`, which would also wrap the 404 fallback).
    //
    // What lives here vs in `protected`:
    //   * Anything project-scoped (resources, deployments, env, backups, …)
    //     stays in `protected` — each handler self-gates via
    //     `enforce_resource_role` / `enforce_project_role`.
    //   * Anything global-by-design (Docker daemon, compose stacks, system
    //     mutations, S3 storages, sources, registries, alerts, audit log,
    //     server mutations) moves here so non-Admin users can't reach it.
    let admin_only = Router::new()
        .route("/users", get(users::list))
        .route("/users/invite", post(users::invite))
        .route("/users/{id}", put(users::update).delete(users::remove))
        // Ad-hoc Tasks — admin only (in MVP). Future RBAC may scope by
        // server/project; for now they share the user-management gate.
        .route(
            "/tasks/templates",
            get(tasks::templates_list).post(tasks::templates_create),
        )
        .route(
            "/tasks/templates/{id}",
            get(tasks::templates_get)
                .put(tasks::templates_update)
                .delete(tasks::templates_delete),
        )
        .route("/tasks/runs", get(tasks::runs_list).post(tasks::runs_start))
        .route("/tasks/runs/{id}", get(tasks::runs_get))
        .route("/tasks/runs/{id}/cancel", post(tasks::runs_cancel))
        // User-defined cron schedules (Stage 3 of the Semaphore-inspired rollout).
        .route("/schedules", get(schedules::list).post(schedules::create))
        .route(
            "/schedules/{id}",
            get(schedules::get)
                .put(schedules::update)
                .delete(schedules::remove),
        )
        .route("/schedules/{id}/run", post(schedules::run_now))
        .route("/schedules/{id}/enable", post(schedules::enable))
        .route("/schedules/{id}/disable", post(schedules::disable))
        .route("/schedules/validate-cron", post(schedules::validate_cron))
        // Audit log — admin-only (was leaking to any authenticated user).
        .route("/audit/events", get(audit::list_events))
        // Docker daemon endpoints — not project-bound.
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
        .route("/events/ws", get(events::events_ws))
        .route("/images", get(images::list))
        .route("/images/{id}", delete(images::remove))
        // Docker Compose stacks — global ops without per-project ownership.
        .route("/stacks", get(compose::list).post(compose::create))
        .route(
            "/stacks/{id}",
            get(compose::get)
                .put(compose::update)
                .delete(compose::remove),
        )
        .route("/stacks/{id}/deploy", post(compose::deploy))
        .route("/stacks/{id}/down", post(compose::down))
        // Stateless migration — moves a locally-owned compose stack
        // to a federated peer (Этап 4). Refuses stacks with named or
        // anonymous volumes; bind mounts are tolerated. The handler
        // acquires a row-level lock (migration 56) so two operators
        // can't fire the pipeline concurrently for the same stack.
        .route("/stacks/{id}/migrate", post(migration::migrate_stack))
        // Docker networks — system-level.
        .route("/networks", get(networks::list).post(networks::create))
        .route("/networks/{id}", delete(networks::delete))
        // Registry credentials, npm registry admin.
        .route(
            "/registries",
            get(registries::list).post(registries::create),
        )
        .route(
            "/registries/{id}",
            put(registries::update).delete(registries::remove),
        )
        .route("/registries/{id}/test", post(registries::test))
        .route(
            "/registry/settings",
            get(registry_settings::get).put(registry_settings::update),
        )
        .route(
            "/registry/proxy/packages",
            get(registry_settings::proxy_packages_list),
        )
        .route(
            "/registry/proxy/packages/{name}/pin",
            put(registry_settings::proxy_pin_toggle),
        )
        .route(
            "/registry/proxy/packages/{name}/fetch",
            post(registry_settings::proxy_fetch_version),
        )
        .route(
            "/registry/packages/{package}/dist-tags/{tag}",
            put(registry_admin::set_dist_tag).delete(registry_admin::remove_dist_tag),
        )
        .route(
            "/registry/packages/{package}/versions/{version}/deprecate",
            post(registry_admin::deprecate_version),
        )
        .route(
            "/registry/packages/{package}/versions/{version}",
            delete(registry_admin::delete_version),
        )
        .route(
            "/registry/packages/{package}",
            delete(registry_admin::delete_package),
        )
        // Git sources + S3 storages — admin-managed integrations.
        .route("/sources", get(sources::list).post(sources::create))
        .route("/sources/{id}", get(sources::get).delete(sources::remove))
        .route("/sources/{id}/repos", get(sources::list_repos))
        .route(
            "/sources/{id}/branches/{*repo}",
            get(sources::list_branches),
        )
        .route("/sources/{id}/file", get(sources::get_file))
        .route("/sources/github/manifest", get(sources::github_manifest))
        .route("/s3", get(s3::list).post(s3::create))
        .route("/s3/{id}", put(s3::update).delete(s3::remove))
        .route("/s3/{id}/test", post(s3::test))
        // Server mutations (server creation/destruction is admin's job).
        // Read-only server endpoints stay in `protected` so non-admins can
        // see what infrastructure exists in the UI.
        .route("/servers", post(servers::create))
        .route("/servers/install-script", get(servers::install_script))
        .route("/servers/{id}", delete(servers::remove))
        .route("/servers/{id}/name", put(servers::rename))
        .route("/servers/{id}/test", post(servers::test_connection))
        .route("/servers/{id}/rotate", post(servers::rotate_token))
        // Primary-side: store the plaintext federation token the operator
        // copied from a peer's federation_tokens settings page. Only
        // meaningful for kind='peer' rows; agents don't mint these.
        .route(
            "/servers/{id}/federation-token",
            put(servers::set_federation_token),
        )
        // Peer-side: CRUD over this peer's federation_tokens — the
        // tokens it mints so a primary can drive /api/v1/agent/*.
        // Plain admin-level routes (require_auth above) — the actual
        // federation surface they gate sits on a separate auth path.
        .route(
            "/federation-tokens",
            get(federation_tokens::list).post(federation_tokens::create),
        )
        .route("/federation-tokens/{id}", delete(federation_tokens::revoke))
        .route("/servers/{id}/deploy", post(servers::deploy_to_server))
        .route("/servers/{id}/stop", post(servers::stop_on_server))
        .route("/servers/{id}/promote-bundle", get(promote::bundle))
        .route("/servers/{id}/promote", post(promote::trigger))
        // System mutations (cleanup/update/timezone/logs).
        .route("/system/cleanup", post(system::cleanup))
        .route(
            "/system/cleanup-settings",
            get(system::cleanup_settings_get).put(system::cleanup_settings_update),
        )
        .route("/system/update", post(system::update_now))
        .route(
            "/system/update-settings",
            get(system::update_settings).put(system::save_update_settings),
        )
        .route(
            "/system/timezone",
            get(system::get_timezone).put(system::set_timezone),
        )
        .route("/system/logs", get(system_logs::snapshot))
        .route("/system/logs/units", get(system_logs::units_list))
        .route("/system/logs/ws", get(system_logs::stream_ws))
        // Proxy / Traefik administration — server-wide.
        .route("/proxy/enable", post(proxy::enable))
        .route("/proxy/disable", post(proxy::disable))
        .route("/proxy/settings", put(proxy::update_settings))
        .route("/proxy/update", post(proxy::update))
        // Alerts + notifications — admin manages alert rules and channels.
        .route("/alerts", get(alerts::list).post(alerts::create))
        .route("/alerts/events", get(alerts::events_feed))
        .route(
            "/alerts/{id}",
            get(alerts::get).put(alerts::update).delete(alerts::remove),
        )
        .route("/alerts/{id}/toggle", post(alerts::toggle))
        .route("/alerts/{id}/test", post(alerts::test))
        .route("/alerts/{id}/events", get(alerts::rule_events))
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
        // Federation refresh (admin-triggered).
        .route("/federation/sync", post(federation::refresh_now))
        // Write-federation passthroughs. Operator clicks Deploy/Down/
        // Restart on a federated stack card → we look up the paired
        // peer's federation token and forward via write_client.
        .route(
            "/federation/peer/{server_id}/stacks",
            post(federation::peer_create_stack),
        )
        .route(
            "/federation/peer/{server_id}/stacks/{stack_id}/deploy",
            post(federation::peer_deploy_stack),
        )
        .route(
            "/federation/peer/{server_id}/stacks/{stack_id}/down",
            post(federation::peer_down_stack),
        )
        .route(
            "/federation/peer/{server_id}/stacks/{stack_id}/restart",
            post(federation::peer_restart_stack),
        )
        .route(
            "/federation/peer/{server_id}/stacks/{stack_id}/logs",
            get(federation::peer_stack_logs),
        )
        .route(
            "/federation/peer/{server_id}/stacks/{stack_id}/logs/ws-info",
            get(federation::peer_stack_logs_ws_info),
        )
        .route(
            "/federation/peer/{server_id}/stacks/{stack_id}/release",
            post(federation::peer_release_stack),
        )
        // Railpack auto-build admin settings.
        .route(
            "/admin/settings/railpack",
            get(settings_railpack::get).put(settings_railpack::put),
        )
        .route_layer(axum::middleware::from_fn(require_global_admin));

    // Owner-only sub-router — highest-impact actions: global role changes,
    // WireGuard mesh control, cross-core federation grants.
    let owner_only = Router::new()
        .route("/users/{id}/role", put(users::change_role))
        // WireGuard mesh — host-level encrypted overlay across all peers.
        .route(
            "/network/mesh",
            get(network::get_mesh).put(network::put_mesh),
        )
        .route("/network/mesh/enable", post(network::enable_mesh))
        .route("/network/mesh/disable", post(network::disable_mesh))
        .route("/network/mesh/configure", post(network::configure_mesh))
        // Pre-flight: ask every node whether pier-net-helper is reachable
        // so the Enable Mesh wizard can refuse to start with missing
        // helpers instead of failing halfway through configure.
        .route("/network/mesh/preflight", get(network::peer_preflight))
        // Mesh service-DNS CRUD (Etap 3.2). Operators register logical
        // names (`db`, `cache`) that the deploy pipeline injects as
        // `<name>.mesh` extra_hosts entries so consumer stacks don't
        // hard-code per-node hostnames. Auto-redeploy on change is
        // wired in a follow-up commit.
        .route(
            "/network/service-dns",
            get(service_dns::list).post(service_dns::create),
        )
        .route(
            "/network/service-dns/{name}",
            put(service_dns::update).delete(service_dns::remove),
        )
        // Federation grants — tokens that authorize another pier-core to
        // control this one. Owner-only because they grant Admin-equivalent
        // reach over every resource here.
        .route("/grants", get(grants::list).post(grants::create))
        .route("/grants/{id}", delete(grants::revoke))
        .route_layer(axum::middleware::from_fn(require_global_owner));

    let protected =
        protected
            .merge(admin_only)
            .merge(owner_only)
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_auth,
            ));

    // Write-federation surface — completely separate auth path
    // (X-Pier-Federation, not sessions/Bearer). Mounted at
    // /api/v1/agent/* via a second nest below; deliberately *not*
    // merged into `protected` so a federation token can never
    // accidentally satisfy require_auth or vice versa.
    let federation_agent = Router::new()
        .route(
            "/stacks",
            get(federation_agent::list_stacks).post(federation_agent::create_stack),
        )
        .route(
            "/stacks/{id}",
            get(federation_agent::get_stack)
                .put(federation_agent::update_stack)
                .delete(federation_agent::delete_stack),
        )
        .route("/stacks/{id}/deploy", post(federation_agent::deploy_stack))
        .route("/stacks/{id}/down", post(federation_agent::down_stack))
        .route(
            "/stacks/{id}/restart",
            post(federation_agent::restart_stack),
        )
        .route("/stacks/{id}/logs", get(federation_agent::stack_logs))
        .route("/stacks/{id}/logs/ws", get(federation_agent::stack_logs_ws))
        .route("/release/{stack_id}", post(federation_agent::release_stack))
        .route("/rotate-token", post(federation_agent::rotate_token))
        .layer(axum::middleware::from_fn_with_state(
            state,
            crate::auth::federation::require_federation,
        ));

    Router::new()
        // Health at root level for easy monitoring
        .route("/health", get(health))
        // Public retrofit installer for pier-net-helper. Lives at root
        // so the curl-pipe-bash convention works (`curl -fsSL
        // https://core/install-helper.sh | bash`). Unauthenticated by
        // design — same trust model as the existing agent install
        // script. See [`install::install_helper_script`].
        .route("/install-helper.sh", get(install::install_helper_script))
        .nest("/api/v1", public.merge(protected))
        .nest("/api/v1/agent", federation_agent)
}
