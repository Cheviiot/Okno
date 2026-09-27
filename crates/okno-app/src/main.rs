// No console window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

//! Okno desktop application.

mod app;
mod chrome;
mod clipboard;
mod files_ui;
mod host;
mod keys;
mod notify;
mod picker;
mod secrets;
mod session;
mod terminal_ui;

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
    // Wayland app id / X11 class, matching the desktop entry.
    let _ = slint::set_xdg_app_id("io.github.cheviiot.okno");
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
        DECIMAL_COMMA.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = slint::select_bundled_translation("ru") {
            tracing::warn!("Russian translation unavailable: {e}");
        }
    }
}

static DECIMAL_COMMA: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether numbers use a decimal comma (the Russian translation is active).
pub fn uses_decimal_comma() -> bool {
    DECIMAL_COMMA.load(std::sync::atomic::Ordering::Relaxed)
}

/// Opens a link or folder with the default application.
pub fn open_url(url: &str) {
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    match std::process::Command::new(program).arg(url).spawn() {
        // Reap the helper so it does not linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!("cannot open {url}: {e}"),
    }
}
