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
    }
}
