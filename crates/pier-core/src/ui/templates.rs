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
