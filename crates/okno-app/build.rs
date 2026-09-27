//! Compiles the Slint UI with the widget kit of the target platform:
//! Adwaita on Linux, Fluent on Windows. `OKNO_UI_KIT=adwaita|fluent`
//! overrides the choice, e.g. to preview the Windows look on Linux.

use std::collections::HashMap;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=OKNO_UI_KIT");
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let kit = std::env::var("OKNO_UI_KIT")
        .unwrap_or_else(|_| if target_os == "windows" { "fluent".into() } else { "adwaita".into() });
    assert!(matches!(kit.as_str(), "adwaita" | "fluent"), "OKNO_UI_KIT must be adwaita or fluent");

    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let library = HashMap::from([("kit".to_owned(), manifest.join("ui/kit").join(&kit))]);
    // std-widgets (scroll bars, combo box) closest to each kit.
    let style = if kit == "fluent" { "fluent" } else { "cosmic" };
    let config = slint_build::CompilerConfiguration::new()
        .with_style(style.into())
        .with_library_paths(library)
        .with_bundled_translations(manifest.join("po"))
        .with_default_translation_context(slint_build::DefaultTranslationContext::None);
    slint_build::compile_with_config("ui/app.slint", config).expect("Slint UI compiles");
}
