//! Okno desktop application.

mod app;
mod chrome;
mod host;
mod keys;
mod session;

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("OKNO_LOG").unwrap_or_else(|_| "warn,arboard=error".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(4).build()?;
    // UI code starts tokio tasks too (session readers); give it the context.
    let _context = runtime.enter();

    // The winit backend is required for raw keyboard events; SLINT_BACKEND
    // (e.g. winit-software) still picks the renderer.
    if std::env::var_os("SLINT_BACKEND").is_none() {
        slint::BackendSelector::new().backend_name("winit".into()).select()?;
    }
    let app = app::App::new(runtime.handle().clone())?;
    app.run()?;
    Ok(())
}

/// Uses the Russian translation when the system language is Russian;
/// `OKNO_LANG=en` or `OKNO_LANG=ru` overrides. Must run after the first
/// component is created.
pub fn select_language() {
    let lang = std::env::var("OKNO_LANG").ok().or_else(sys_locale::get_locale).unwrap_or_default();
    if lang.to_ascii_lowercase().starts_with("ru") {
        if let Err(e) = slint::select_bundled_translation("ru") {
            tracing::warn!("Russian translation unavailable: {e}");
        }
    }
}
