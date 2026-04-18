pub mod pages;
pub mod templates;

use axum::routing::get;
use axum::Router;

use crate::auth::middleware::require_auth;
use crate::state::SharedState;

/// Build the UI router serving HTML pages and static assets.
pub fn ui_router(state: SharedState) -> Router<SharedState> {
    let public = Router::new()
        .route("/login", get(pages::login_page))
        .route("/setup", get(pages::setup_page))
        .route("/logout", get(pages::logout))
        .route("/static/{*path}", get(pages::static_file));

    let protected = Router::new()
        .route("/", get(pages::dashboard))
        .route("/projects", get(pages::projects_list))
        .route("/projects/{id}", get(pages::project_detail))
        .route("/servers", get(pages::servers_list))
        .route("/servers/{id}", get(pages::server_detail))
        .route("/updates", get(pages::updates_page))
        .route("/alerts", get(pages::alerts_page))
        .route("/sources", get(pages::sources_list))
        .route("/sources/{id}", get(pages::source_detail))
        .route("/s3", get(pages::s3_list))
        .route("/settings", get(pages::settings_page))
        .route("/domains", get(pages::domains_page))
        .route("/proxy", get(pages::proxy_page))
        .route("/networks", get(pages::networks_page))
        .route("/canvas", get(pages::canvas_page))
        // Resources (catalog)
        .route("/resources/new", get(pages::resources_catalog))
        .route("/resources/new/{catalog_id}", get(pages::resources_create))
        .route("/resources/{id}", get(pages::resource_detail))
        // Legacy routes (backward compatibility)
        .route("/containers", get(pages::containers_list))
        .route("/containers/{id}", get(pages::container_detail))
        .route("/images", get(pages::images_list))
        .route("/stacks", get(pages::stacks_list))
        .route("/stacks/new", get(pages::stack_new))
        .route("/stacks/{id}", get(pages::stack_edit))
        .layer(axum::middleware::from_fn_with_state(state, require_auth));

    public.merge(protected)
}
