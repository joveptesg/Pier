use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};

use crate::auth::middleware::AuthUser;
use crate::error::AppError;
use crate::state::SharedState;

type PageResult = Result<Response, AppError>;

/// Helper to render a template with context.
fn render(state: &SharedState, template: &str, ctx: minijinja::Value) -> PageResult {
    let tmpl = state
        .templates
        .get_template(template)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Template '{template}': {e}")))?;
    let html = tmpl
        .render(ctx)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Render '{template}': {e}")))?;
    Ok(Html(html).into_response())
}

// ── Auth Pages ──────────────────────────────────────────────

/// GET /login
pub async fn login_page(State(state): State<SharedState>) -> PageResult {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let count: u32 = db.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
    drop(db);

    if count == 0 {
        return Ok(Redirect::to("/setup").into_response());
    }

    render(&state, "login.html", minijinja::context! {})
}

/// GET /setup
pub async fn setup_page(State(state): State<SharedState>) -> PageResult {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let count: u32 = db.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
    drop(db);

    if count > 0 {
        return Ok(Redirect::to("/login").into_response());
    }

    render(&state, "setup.html", minijinja::context! {})
}

/// GET /logout
pub async fn logout(State(state): State<SharedState>) -> impl IntoResponse {
    let cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        state.config.session_cookie,
    );
    (
        [(axum::http::header::SET_COOKIE, cookie)],
        Redirect::to("/login"),
    )
}

// ── Protected Pages ─────────────────────────────────────────

/// GET /
pub async fn dashboard(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "dashboard.html",
        minijinja::context! { user => user.username, page => "dashboard" },
    )
}

/// GET /containers
pub async fn containers_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "containers/list.html",
        minijinja::context! { user => user.username, page => "containers" },
    )
}

/// GET /containers/{id}
pub async fn container_detail(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> PageResult {
    render(
        &state,
        "containers/detail.html",
        minijinja::context! { user => user.username, page => "containers", container_id => id },
    )
}

/// GET /images
pub async fn images_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "images/list.html",
        minijinja::context! { user => user.username, page => "images" },
    )
}

/// GET /stacks
pub async fn stacks_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "compose/list.html",
        minijinja::context! { user => user.username, page => "stacks" },
    )
}

/// GET /stacks/new
pub async fn stack_new(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "compose/editor.html",
        minijinja::context! { user => user.username, page => "stacks", mode => "create" },
    )
}

/// GET /stacks/{id}
pub async fn stack_edit(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> PageResult {
    render(
        &state,
        "compose/editor.html",
        minijinja::context! { user => user.username, page => "stacks", mode => "edit", stack_id => id },
    )
}

/// GET /servers
pub async fn servers_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "servers/list.html",
        minijinja::context! { user => user.username, page => "servers" },
    )
}

/// GET /servers/{id}
pub async fn server_detail(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> PageResult {
    render(
        &state,
        "servers/detail.html",
        minijinja::context! { user => user.username, page => "servers", server_id => id },
    )
}

/// GET /updates
pub async fn updates_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "settings/updates.html",
        minijinja::context! { user => user.username, page => "updates", version => env!("CARGO_PKG_VERSION") },
    )
}

/// GET /alerts
pub async fn alerts_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "settings/alerts.html",
        minijinja::context! { user => user.username, page => "alerts" },
    )
}

/// GET /settings
pub async fn settings_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "settings/index.html",
        minijinja::context! { user => user.username, page => "settings", version => env!("CARGO_PKG_VERSION") },
    )
}

/// GET /domains
pub async fn domains_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "domains/list.html",
        minijinja::context! { user => user.username, page => "domains" },
    )
}

/// GET /proxy
pub async fn proxy_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "proxy/settings.html",
        minijinja::context! { user => user.username, page => "proxy" },
    )
}

/// GET /canvas
pub async fn canvas_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "canvas.html",
        minijinja::context! { user => user.username, page => "canvas" },
    )
}

/// GET /networks
pub async fn networks_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "networks/list.html",
        minijinja::context! { user => user.username, page => "networks" },
    )
}

/// GET /projects
pub async fn projects_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "projects/list.html",
        minijinja::context! { user => user.username, page => "projects" },
    )
}

/// GET /projects/{id}
pub async fn project_detail(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> PageResult {
    render(
        &state,
        "projects/detail.html",
        minijinja::context! { user => user.username, page => "projects", project_id => id },
    )
}

// ── Resource Pages ──────────────────────────────────────────

/// GET /resources/new
pub async fn resources_catalog(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "resources/catalog.html",
        minijinja::context! { user => user.username, page => "projects" },
    )
}

/// GET /resources/new/{catalog_id}
pub async fn resources_create(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(catalog_id): Path<String>,
) -> PageResult {
    render(
        &state,
        "resources/create.html",
        minijinja::context! { user => user.username, page => "projects", catalog_id => catalog_id },
    )
}

/// GET /resources/{id}
pub async fn resource_detail(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> PageResult {
    render(
        &state,
        "resources/detail.html",
        minijinja::context! { user => user.username, page => "projects", resource_id => id },
    )
}

// ── Sources Page ────────────────────────────────────────────

/// GET /sources
pub async fn sources_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "sources/list.html",
        minijinja::context! { user => user.username, page => "sources" },
    )
}

/// GET /sources/{id}
pub async fn source_detail(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(id): Path<String>,
) -> PageResult {
    render(
        &state,
        "sources/detail.html",
        minijinja::context! { user => user.username, page => "sources", source_id => id },
    )
}

// ── S3 Storages Page ────────────────────────────────────────

/// GET /s3
pub async fn s3_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "s3/list.html",
        minijinja::context! { user => user.username, page => "s3" },
    )
}

// ── Static Files ────────────────────────────────────────────

/// GET /static/{*path}
pub async fn static_file(Path(path): Path<String>) -> impl IntoResponse {
    use super::templates::{content_type_for, StaticAssets};

    match StaticAssets::get(&path) {
        Some(file) => (
            StatusCode::OK,
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    content_type_for(&path).to_string(),
                ),
                (
                    axum::http::header::CACHE_CONTROL,
                    "public, max-age=31536000, immutable".to_string(),
                ),
            ],
            file.data.to_vec(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
