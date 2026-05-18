use std::collections::HashMap;

use axum::extract::{Path, Query, State};
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
///
/// Three states:
///   1. A user already exists → 302 /login (priority: setup is over).
///   2. Token store is required AND the query token doesn't match → 404
///      (we deliberately don't reveal that a setup form lives here).
///   3. Otherwise → render the form; the token (if any) is threaded into JS
///      so the POST body picks it up automatically.
pub async fn setup_page(
    State(state): State<SharedState>,
    Query(q): Query<HashMap<String, String>>,
) -> PageResult {
    // Pull both the user count and the (optional) proxy auto-start error
    // in one DB lock acquisition so we don't take the mutex twice.
    let (count, proxy_error) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let count: u32 = db.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        let proxy_error: Option<String> = db
            .query_row(
                "SELECT value FROM settings WHERE key = 'proxy.last_error'",
                [],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .filter(|v| !v.is_empty());
        (count, proxy_error)
    };

    if count > 0 {
        return Ok(Redirect::to("/login").into_response());
    }

    let supplied = q.get("token").map(|s| s.as_str()).unwrap_or("");
    if state.setup_token.is_required() && !state.setup_token.matches(supplied) {
        return Ok((StatusCode::NOT_FOUND, "Not Found").into_response());
    }

    render(
        &state,
        "setup.html",
        minijinja::context! { setup_token => supplied, proxy_error => proxy_error },
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

/// GET /account/security — TOTP enrollment + recovery code management.
pub async fn security_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "account/security.html",
        minijinja::context! { user => user.username, page => "settings" },
    )
}

/// GET /audit — paginated, filterable view of auth events.
pub async fn audit_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "audit/index.html",
        minijinja::context! { user => user.username, page => "settings" },
    )
}

/// GET /team — global user list + invite UI (Admin+).
///
/// The route stays accessible to any authenticated user; the API calls
/// the page issues (`/api/v1/users`, `/api/v1/users/invite`) are themselves
/// gated by `require_global_admin`, so a non-admin lands on the page and
/// just sees an error toast.
pub async fn team_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "team/index.html",
        minijinja::context! { user => user.username, page => "team" },
    )
}

/// GET /invitations/{token} — public page to accept an invitation. Renders
/// even for unknown tokens; the JS-side fetch decides whether to show the
/// form or the "link expired" view.
pub async fn invitation_accept_page(
    State(state): State<SharedState>,
    Path(token): Path<String>,
) -> PageResult {
    render(
        &state,
        "invitations/accept.html",
        minijinja::context! { token => token },
    )
}

/// GET /tasks — Ad-hoc Tasks dashboard (templates + recent runs).
pub async fn tasks_list_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "tasks/list.html",
        minijinja::context! { user => user.username, page => "tasks" },
    )
}

/// GET /tasks/runs/{id} — single task-run detail with live log polling.
pub async fn task_run_detail_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(run_id): Path<String>,
) -> PageResult {
    render(
        &state,
        "tasks/run_detail.html",
        minijinja::context! { user => user.username, page => "tasks", run_id => run_id },
    )
}

/// GET /schedules — user-defined cron schedules dashboard.
pub async fn schedules_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "schedules/list.html",
        minijinja::context! { user => user.username, page => "schedules" },
    )
}

/// GET /settings/external-access — manage tokens that let other pier-cores control this one.
pub async fn external_access_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "settings/external-access.html",
        minijinja::context! { user => user.username, page => "settings" },
    )
}

/// GET /logs — system logs (journalctl) viewer.
pub async fn system_logs(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    // Build the list of installed allow-listed units so the UI can hide
    // selectors for units that aren't installed on this host.
    let mut units: Vec<&'static str> = Vec::new();
    for unit in crate::api::system_logs::ALLOWED_UNITS {
        let installed = tokio::process::Command::new("systemctl")
            .args([
                "list-unit-files",
                &format!("{unit}.service"),
                "--no-legend",
                "--no-pager",
            ])
            .output()
            .await
            .ok()
            .map(|out| {
                out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
            })
            .unwrap_or(false);
        if installed {
            units.push(*unit);
        }
    }

    render(
        &state,
        "settings/logs.html",
        minijinja::context! {
            user => user.username,
            page => "logs",
            units => units,
            default_unit => units.first().copied().unwrap_or("pier"),
        },
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

/// GET /notifications
pub async fn notifications_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "settings/notifications.html",
        minijinja::context! { user => user.username, page => "notifications" },
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

/// GET /registries
pub async fn registries_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    render(
        &state,
        "registries/index.html",
        minijinja::context! { user => user.username, page => "registries" },
    )
}

/// GET /packages — list private npm packages held by the embedded registry.
pub async fn packages_list(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
) -> PageResult {
    let packages = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        crate::registry::db::list_packages(&db, true)?
    };
    render(
        &state,
        "packages/list.html",
        minijinja::context! {
            user => user.username,
            page => "packages",
            packages => packages,
        },
    )
}

/// GET /packages/{name} — detail page for a single npm package.
/// `{name}` is path-encoded for scoped packages (`@scope%2Fname`).
pub async fn package_detail(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(name): Path<String>,
) -> PageResult {
    // Pre-flight: for proxy packages whose upstream blob isn't populated yet
    // (legacy entries from before migration 57, OR a fresh row whose blob
    // got wiped by an unpublish), do a synchronous upstream refresh so the
    // detail page renders real data instead of a misleading "0 versions"
    // state. Failures here are non-fatal — we still render whatever we
    // have in the DB.
    let needs_refresh = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let row: Option<(i64, Option<String>)> = db
            .query_row(
                "SELECT is_proxy, upstream_packument_json FROM npm_packages WHERE name = ?1",
                [&name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        matches!(row, Some((p, blob)) if p != 0 && blob.as_deref().unwrap_or("").is_empty())
    };
    if needs_refresh {
        let cfg = {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            crate::registry::upstream::load_config(&db)
        };
        if cfg.enabled {
            match crate::registry::upstream::fetch_packument(&cfg.upstream_url, &name, false).await
            {
                Ok(Some(up)) => {
                    let db = state
                        .db
                        .lock()
                        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                    if let Err(e) = crate::registry::db::upsert_proxy_packument(
                        &db,
                        &name,
                        &up.body,
                        up.etag.as_deref(),
                    ) {
                        tracing::warn!(%name, "detail-page proxy refresh upsert failed: {e:#}");
                    }
                }
                Ok(None) => {
                    tracing::info!(%name, "detail-page proxy refresh: upstream 404");
                }
                Err(e) => {
                    tracing::warn!(%name, "detail-page proxy refresh failed: {e:#}");
                }
            }
        }
    }

    let (summary, versions, manifest_only_count, dist_tags, readme_md) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let summaries = crate::registry::db::list_packages(&db, false)?;
        let summary = summaries.into_iter().find(|p| p.name == name);
        // For proxy packages the upstream packument enumerates the entire
        // version history (thousands of rows for popular packages). Hide the
        // manifest-only ones from the table — only show downloaded versions.
        // Private packages always have tarball_size > 0 so this is a no-op
        // for them.
        let is_proxy = summary.as_ref().map(|s| s.is_proxy).unwrap_or(false);
        let versions =
            crate::registry::db::list_versions_with_deprecation(&db, &name, is_proxy)?;
        let manifest_only_count = if is_proxy {
            crate::registry::db::count_manifest_only_versions(&db, &name)?
        } else {
            0
        };
        let dist_tags = crate::registry::db::load_dist_tags(&db, &name)?.unwrap_or_default();
        let readme_md = crate::registry::db::load_readme(&db, &name)?;
        (summary, versions, manifest_only_count, dist_tags, readme_md)
    };
    let Some(summary) = summary else {
        return Err(AppError::NotFound(format!("package {name}")));
    };

    // Render markdown to HTML inside the binary, then sanitise so a hostile
    // publish can't slip `<script>` or `onerror` past us into the panel DOM.
    let readme_html = readme_md.as_deref().map(|md| {
        use pulldown_cmark::{html, Parser};
        let parser = Parser::new(md);
        let mut raw = String::new();
        html::push_html(&mut raw, parser);
        ammonia::clean(&raw)
    });

    render(
        &state,
        "packages/detail.html",
        minijinja::context! {
            user => user.username,
            page => "packages",
            package => summary,
            versions => versions,
            manifest_only_count => manifest_only_count,
            dist_tags => dist_tags,
            readme_html => readme_html,
        },
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

/// GET /login/cli/{session_id} — confirmation page for `npm login --auth-type=web`.
/// Reached when the CLI opens its `loginUrl` in a browser. Requires a logged-in
/// panel session; the actual token is minted by
/// `POST /api/v1/account/cli-login/{session_id}/authorize`.
pub async fn cli_login_page(
    State(state): State<SharedState>,
    axum::Extension(user): axum::Extension<AuthUser>,
    Path(session_id): Path<String>,
) -> PageResult {
    use rusqlite::OptionalExtension;

    // Pre-load session metadata so the template can show hostname / peer-IP /
    // user-agent — lets the operator double-check they're authorising the
    // right CLI before clicking the button.
    struct SessionInfo {
        hostname: String,
        status: String,
        peer_ip: Option<String>,
        user_agent: Option<String>,
        expires_at: i64,
    }
    let row: Option<SessionInfo> = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        db.query_row(
            "SELECT hostname, status, peer_ip, user_agent, expires_at
             FROM npm_login_sessions WHERE session_id = ?1",
            [&session_id],
            |row| {
                Ok(SessionInfo {
                    hostname: row.get(0)?,
                    status: row.get(1)?,
                    peer_ip: row.get::<_, Option<String>>(2)?,
                    user_agent: row.get::<_, Option<String>>(3)?,
                    expires_at: row.get(4)?,
                })
            },
        )
        .optional()?
    };

    let Some(info) = row else {
        return Err(AppError::NotFound(format!("login session {session_id}")));
    };
    let SessionInfo {
        hostname,
        status,
        peer_ip,
        user_agent,
        expires_at,
    } = info;

    let now = chrono::Utc::now().timestamp();
    let effective_status = if now > expires_at && status == "pending" {
        "expired".to_string()
    } else {
        status
    };

    render(
        &state,
        "cli_login.html",
        minijinja::context! {
            user => user.username,
            page => "cli_login",
            session_id => session_id,
            hostname => hostname,
            status => effective_status,
            peer_ip => peer_ip,
            user_agent => user_agent,
            expires_at => expires_at,
        },
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
