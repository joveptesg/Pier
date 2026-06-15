//! Internationalization (i18n) for the admin panel.
//!
//! The translation catalog is embedded at compile time by the
//! `rust_i18n::i18n!("locales", fallback = "en")` invocation in the crate root
//! (`main.rs`). English is the source locale and the fallback for any missing
//! key or unknown locale.
//!
//! Request flow:
//!   1. [`locale_layer`] runs as the outermost middleware. It resolves the best
//!      locale for the request and binds it to the [`CURRENT_LOCALE`] task-local
//!      for the lifetime of the downstream handler.
//!   2. Templates call the MiniJinja `t()` function (registered in
//!      `ui::templates::init_templates`), which delegates to [`translate`] and
//!      reads the locale back from the task-local.
//!
//! A `tokio::task_local!` is used deliberately — not a `thread_local!` nor
//! `rust_i18n::set_locale()`. The locale must follow the request across `.await`
//! points (ruling out thread-locals, which a task can migrate off of) and must
//! never leak between concurrent requests sharing a worker thread (ruling out
//! the process-global locale that `set_locale` mutates).

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;

tokio::task_local! {
    /// The resolved locale for the in-flight request. Bound by [`locale_layer`]
    /// for the duration of the downstream handler; read by [`current_locale`].
    pub static CURRENT_LOCALE: String;
}

/// The locale active for the current request, or `"en"` when called outside a
/// request scope (startup, background tasks, unit tests).
pub fn current_locale() -> String {
    CURRENT_LOCALE
        .try_with(|loc| loc.clone())
        .unwrap_or_else(|_| "en".to_string())
}

/// Pick the best locale for a request from its `Accept-Language` header,
/// constrained to the locales actually compiled into the catalog, falling back
/// to the source locale `"en"`.
///
/// Today only `en` ships, so this always yields `"en"`. The negotiation is in
/// place so that dropping a second `*.yml` file is the only change needed to
/// enable browser-driven locale selection. Cookie / stored-preference sources
/// are layered on later (see the i18n plan's "deferred" section).
fn resolve_locale(req: &Request) -> String {
    // `available_locales!()` may yield `Vec<String>` or `Vec<&str>` depending on
    // the rust-i18n version; normalise to owned strings, then borrow as the
    // `&[&str]` that `accept_language::intersection` expects.
    let owned: Vec<String> = rust_i18n::available_locales!()
        .iter()
        .map(|loc| loc.to_string())
        .collect();
    let supported: Vec<&str> = owned.iter().map(|loc| loc.as_str()).collect();

    let header = req
        .headers()
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");

    accept_language::intersection(header, &supported)
        .first()
        .map(|loc| loc.to_string())
        .unwrap_or_else(|| "en".to_string())
}

/// Axum middleware: resolve the request locale and bind it to [`CURRENT_LOCALE`]
/// for everything downstream — auth middleware, page handlers, and response
/// rendering (including localized error responses, once those land).
///
/// Installed as the outermost layer in `main.rs` so the binding is already live
/// before any inner middleware or handler runs.
pub async fn locale_layer(req: Request, next: Next) -> Response {
    let locale = resolve_locale(&req);
    CURRENT_LOCALE.scope(locale, next.run(req)).await
}

/// Translate an error-message key for the current request locale.
///
/// Used at `AppError` construction sites so error responses are localizable:
/// `AppError::BadRequest(te("errors.username_required"))`. Resolution happens
/// inside the request task, so [`current_locale`] is the caller's locale.
pub fn te(key: &str) -> String {
    let locale = current_locale();
    rust_i18n::t!(key, locale = &locale).to_string()
}

/// Like [`te`] but substitutes `%{name}` placeholders from `args`, for error
/// messages that interpolate runtime values:
/// `te_args("errors.project_not_found", &[("id", &id)])`.
pub fn te_args(key: &str, args: &[(&str, &str)]) -> String {
    let locale = current_locale();
    let mut rendered = rust_i18n::t!(key, locale = &locale).to_string();
    for (name, value) in args {
        rendered = rendered.replace(&format!("%{{{name}}}"), value);
    }
    rendered
}

/// Backing implementation of the MiniJinja `t()` function.
///
/// - `{{ t("key") }}` looks `key` up in the catalog for the current request
///   locale.
/// - `{{ t("key", name=value) }}` additionally substitutes `%{name}`
///   placeholders from the keyword arguments.
///
/// Interpolation is done here (not via `t!`'s own `name = value` form) because
/// the placeholder names are only known at template-render time, not at compile
/// time. We therefore fetch the raw translated string with its `%{...}`
/// placeholders intact and replace them ourselves.
pub fn translate(key: &str, kwargs: minijinja::value::Kwargs) -> Result<String, minijinja::Error> {
    let locale = current_locale();
    let mut rendered = rust_i18n::t!(key, locale = &locale).to_string();

    for name in kwargs.args() {
        let value: minijinja::Value = kwargs.get(name)?;
        rendered = rendered.replace(&format!("%{{{name}}}"), &value.to_string());
    }
    // Surfaces caller typos: a kwarg that matched no placeholder is an error.
    kwargs.assert_all_used()?;

    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- rust-i18n runtime-API guarantees the bridge depends on (the Step-1
    // spike, kept as regression coverage). ---

    /// `t!` accepts a runtime `&str` key and a runtime locale expression.
    #[test]
    fn t_accepts_runtime_key_and_locale() {
        let key = String::from("test_fixtures.plain");
        let loc = String::from("en");
        let got = rust_i18n::t!(key.as_str(), locale = loc.as_str());
        assert_eq!(got, "Spike works");
    }

    /// With no interpolation vars passed, `t!` leaves `%{...}` intact so we can
    /// substitute ourselves.
    #[test]
    fn t_leaves_placeholders_untouched_when_no_vars() {
        let raw = rust_i18n::t!("test_fixtures.interp", locale = "en");
        assert!(
            raw.contains("%{name}"),
            "expected raw placeholder, got: {raw}"
        );
    }

    /// Unknown locales fall back to the source locale.
    #[test]
    fn t_falls_back_to_source_locale() {
        let got = rust_i18n::t!("test_fixtures.plain", locale = "zz-XX");
        assert_eq!(got, "Spike works");
    }

    /// A real catalog key resolves to its English source string.
    #[test]
    fn real_key_resolves() {
        assert_eq!(rust_i18n::t!("nav.dashboard", locale = "en"), "Dashboard");
    }

    // --- Bridge + negotiation behaviour. ---

    /// Outside a request scope, the current locale defaults to `en`.
    #[test]
    fn current_locale_defaults_to_en_outside_request() {
        assert_eq!(current_locale(), "en");
    }

    /// The MiniJinja `t()` function resolves a key and interpolates kwargs.
    #[test]
    fn minijinja_t_resolves_and_interpolates() {
        let mut env = minijinja::Environment::new();
        env.add_function("t", translate);

        let plain = env
            .render_str("{{ t('test_fixtures.plain') }}", minijinja::context! {})
            .unwrap();
        assert_eq!(plain, "Spike works");

        let interp = env
            .render_str(
                "{{ t('test_fixtures.interp', name='World') }}",
                minijinja::context! {},
            )
            .unwrap();
        assert_eq!(interp, "Hello, World");
    }

    /// `te` resolves an error key, and `te_args` substitutes `%{name}`.
    #[test]
    fn te_resolves_and_interpolates() {
        assert_eq!(te("errors.unauthorized"), "Unauthorized");
        assert_eq!(
            te_args("errors.name_conflict", &[("name", "web")]),
            "Resource 'web' already exists"
        );
    }

    /// With only `en` compiled in, any `Accept-Language` resolves to `en`, and a
    /// missing/garbage header also resolves to `en`.
    #[test]
    fn resolve_locale_constrained_to_available() {
        let with_header = Request::builder()
            .header(axum::http::header::ACCEPT_LANGUAGE, "ru,fr;q=0.8,en;q=0.5")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(resolve_locale(&with_header), "en");

        let no_header = Request::builder().body(axum::body::Body::empty()).unwrap();
        assert_eq!(resolve_locale(&no_header), "en");
    }
}
