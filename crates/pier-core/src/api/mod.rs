pub mod auth;
pub mod backups;
pub mod canvas;
pub mod catalog;
pub mod databases;
pub mod compose;
pub mod containers;
pub mod deployments;
pub mod domains;
pub mod env;
pub mod images;
pub mod networks;
pub mod projects;
pub mod proxy;
pub mod resources;
pub mod s3;
pub mod servers;
pub mod sources;
pub mod system;
pub mod webhooks;

use axum::extract::State;
use axum::routing::{delete, get, post, put};
use axum::Router;

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
pub fn api_router(state: SharedState) -> Router<SharedState> {
    let public = Router::new()
        .route("/auth/login", post(auth::login))
        .route("/auth/setup", post(auth::setup))
        // Health check
        .route("/health", get(health))
        // Agent heartbeat (token-based auth, not session)
        .route("/servers/heartbeat", post(servers::heartbeat))
        // Webhooks (public — GitHub/GitLab need to reach these)
        .route("/webhooks/github", post(webhooks::github))
        .route("/webhooks/gitlab", post(webhooks::gitlab))
        // GitHub App manifest callback (public — GitHub redirects here)
        .route(
            "/sources/github/callback",
            get(sources::github_callback),
        );

    let protected = Router::new()
        // Auth
        .route("/auth/logout", post(auth::logout))
        .route("/auth/session", get(auth::session_check))
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
        .route("/containers/{id}/stats", get(containers::stats))
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
            "/resources/{id}/port-public",
            put(resources::set_port_public),
        )
        .route(
            "/resources/{id}/network",
            put(resources::set_network),
        )
        .route(
            "/resources/{id}/deployment-logs",
            get(resources::deployment_logs),
        )
        // Git config
        .route(
            "/resources/{id}/git",
            get(resources::get_git_config).put(resources::update_git_config),
        )
        // Deployments (CI/CD pipeline)
        .route("/resources/{id}/deploy", post(deployments::manual_deploy))
        .route("/resources/{id}/rollback", post(deployments::rollback))
        .route("/resources/{id}/deployments", get(deployments::list))
        .route(
            "/resources/{id}/deployments/{dep_id}",
            get(deployments::get),
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
        // Environment Variables
        .route(
            "/resources/{id}/env",
            get(env::get_env).put(env::update_env),
        )
        // Backups
        .route("/resources/{id}/backups", get(backups::list_backups))
        .route(
            "/resources/{id}/backups/schedule",
            get(backups::get_schedule).post(backups::create_schedule),
        )
        .route(
            "/resources/{id}/backups/schedule/{schedule_id}",
            delete(backups::delete_schedule),
        )
        .route(
            "/resources/{id}/backups/trigger",
            post(backups::trigger_backup),
        )
        .route(
            "/backups/{backup_id}/download",
            get(backups::download_backup),
        )
        // Sources
        .route("/sources", get(sources::list).post(sources::create))
        .route("/sources/{id}", get(sources::get).delete(sources::remove))
        .route("/sources/{id}/repos", get(sources::list_repos))
        .route("/sources/github/manifest", get(sources::github_manifest))
        // S3 Storages
        .route("/s3", get(s3::list).post(s3::create))
        .route("/s3/{id}", delete(s3::remove))
        .route("/s3/{id}/test", post(s3::test))
        // Servers
        .route("/servers", get(servers::list).post(servers::create))
        .route("/servers/{id}", delete(servers::remove))
        .route("/servers/{id}/name", put(servers::rename))
        .route("/servers/{id}/test", post(servers::test_connection))
        .route("/servers/{id}/metrics", get(servers::metrics))
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
        // System
        .route("/system/metrics", get(system::metrics))
        .route("/system/docker", get(system::docker_info))
        .layer(axum::middleware::from_fn_with_state(state, require_auth));

    Router::new()
        // Health at root level for easy monitoring
        .route("/health", get(health))
        .nest("/api/v1", public.merge(protected))
}
