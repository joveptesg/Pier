use minijinja::Environment;
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets/templates/"]
pub struct TemplateAssets;

#[derive(RustEmbed)]
#[folder = "assets/static/"]
pub struct StaticAssets;

/// Load all embedded templates into a MiniJinja environment.
pub fn init_templates() -> Environment<'static> {
    let mut env = Environment::new();

    for path in TemplateAssets::iter() {
        if let Some(file) = TemplateAssets::get(&path) {
            if let Ok(content) = std::str::from_utf8(file.data.as_ref()) {
                let _ = env.add_template_owned(path.to_string(), content.to_string());
            }
        }
    }

    // Inject version as a global variable available in all templates
    env.add_global("version", env!("CARGO_PKG_VERSION"));

    // Localization: `{{ t("key") }}` / `{{ t("key", name=value) }}` resolves
    // against the embedded catalog using the current request's locale. See
    // `crate::i18n` for how the locale is bound per request.
    env.add_function("t", crate::i18n::translate);

    tracing::info!("Loaded {} templates", TemplateAssets::iter().count());
    env
}

/// Resolve content type from file extension.
pub fn content_type_for(path: &str) -> &'static str {
    if path.ends_with(".js") {
        "application/javascript"
    } else if path.ends_with(".css") {
        "text/css"
    } else if path.ends_with(".html") {
        "text/html"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".ico") {
        "image/x-icon"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All templates load, the MiniJinja `t()` function resolves catalog keys,
    /// and `extends`/`include` chains render — exercised end-to-end on the two
    /// section-1 screens. Outside a request scope `t()` defaults to `en`.
    #[test]
    fn templates_render_with_localized_strings() {
        let env = init_templates();

        let dashboard = env
            .get_template("dashboard.html")
            .expect("dashboard.html loads")
            .render(minijinja::context! { user => "admin", page => "dashboard" })
            .expect("dashboard.html renders");
        assert!(dashboard.contains("System overview and resource usage"));
        assert!(dashboard.contains("Docker Engine"));
        // Sidebar label from base.html came through the t() bridge too.
        assert!(dashboard.contains("Notifications"));
        // t() embedded inside an Alpine x-text expression is rendered too.
        assert!(dashboard.contains("+ ' running'"));
        // Plural noun from base.html's deploy badge expression.
        assert!(dashboard.contains("'deployments'"));

        let login = env
            .get_template("login.html")
            .expect("login.html loads")
            .render(minijinja::context! {})
            .expect("login.html renders");
        assert!(login.contains("Sign in"));
        assert!(login.contains("Two-factor authentication"));
        // Tagline comes from the auth_base.html parent layout.
        assert!(login.contains("Deploy Anything. Own Everything."));

        let setup = env
            .get_template("setup.html")
            .expect("setup.html loads")
            .render(minijinja::context! { setup_token => "tok" })
            .expect("setup.html renders");
        assert!(setup.contains("Create Admin Account"));
        assert!(setup.contains("Generate strong password"));

        let cli = env
            .get_template("cli_login.html")
            .expect("cli_login.html loads")
            .render(minijinja::context! {
                status => "authorized", session_id => "s", expires_at => 9_999_999_999u64, user => "admin"
            })
            .expect("cli_login.html renders");
        assert!(cli.contains("Authorize CLI"));
        // A sentence carrying inline markup is rendered with `| safe`, so the
        // <code> tag survives instead of being HTML-escaped.
        assert!(cli.contains(r#"<code class="font-mono">.npmrc</code>"#));

        let invite = env
            .get_template("invitations/accept.html")
            .expect("invitations/accept.html loads")
            .render(minijinja::context! { token => "tok" })
            .expect("invitations/accept.html renders");
        assert!(invite.contains("Welcome to Pier"));
        assert!(invite.contains("Account created"));

        for (tpl, needle) in [
            (
                "projects/list.html",
                "Manage projects and standalone resources",
            ),
            ("projects/detail.html", "No services in this project"),
            ("servers/list.html", "Mesh Network"),
            ("servers/detail.html", "Connection Info"),
        ] {
            let out = env
                .get_template(tpl)
                .unwrap_or_else(|_| panic!("{tpl} loads"))
                .render(minijinja::context! { user => "admin", page => "servers" })
                .unwrap_or_else(|e| panic!("{tpl} renders: {e}"));
            assert!(out.contains(needle), "{tpl} should contain {needle:?}");
        }
    }
}
