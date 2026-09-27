//! Client-side window decorations and the system look.
//!
//! With the Adwaita kit the windows have no system frame: the header bar
//! draws the window buttons and calls `WindowOps`, implemented here with
//! winit. The accent colour, dark style and button layout follow GNOME
//! through the XDG Settings portal and update live.

use std::cell::Cell;
use std::rc::Rc;

use slint::winit_030::winit::dpi::PhysicalPosition;
use slint::winit_030::winit::window::ResizeDirection;

use crate::ResizeEdge;

/// Appearance taken from the desktop.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Look {
    pub dark: Option<bool>,
    pub accent: Option<slint::Color>,
    /// GNOME `button-layout`, e.g. `appmenu:minimize,maximize,close`.
    pub button_layout: Option<String>,
}

pub fn resize_direction(edge: ResizeEdge) -> ResizeDirection {
    match edge {
        ResizeEdge::North => ResizeDirection::North,
        ResizeEdge::South => ResizeDirection::South,
        ResizeEdge::East => ResizeDirection::East,
        ResizeEdge::West => ResizeDirection::West,
        ResizeEdge::NorthEast => ResizeDirection::NorthEast,
        ResizeEdge::NorthWest => ResizeDirection::NorthWest,
        ResizeEdge::SouthEast => ResizeDirection::SouthEast,
        ResizeEdge::SouthWest => ResizeDirection::SouthWest,
    }
}

/// Splits a GNOME button layout into the buttons before and after the
/// title, keeping the ones the header bar can draw.
pub fn parse_button_layout(layout: &str) -> (Vec<String>, Vec<String>) {
    let side = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|b| matches!(*b, "minimize" | "maximize" | "close"))
            .map(str::to_owned)
            .collect()
    };
    match layout.split_once(':') {
        Some((start, end)) => (side(start), side(end)),
        None => (Vec::new(), side(layout)),
    }
}

/// Last pointer position in a window, for the window menu.
pub type Cursor = Rc<Cell<PhysicalPosition<f64>>>;

/// Wires `WindowOps` of a component. `$close` runs for the close button.
macro_rules! install {
    ($component:expr, $cursor:expr, $close:expr) => {{
        use slint::ComponentHandle as _;
        use slint::winit_030::WinitWindowAccessor as _;
        let ops = $component.global::<$crate::WindowOps>();
        let weak = $component.as_weak();
        ops.on_drag(move || {
            if let Some(c) = weak.upgrade() {
                c.window().with_winit_window(|w| {
                    let _ = w.drag_window();
                });
            }
        });
        let weak = $component.as_weak();
        ops.on_toggle_maximize(move || {
            if let Some(c) = weak.upgrade() {
                c.window().with_winit_window(|w| w.set_maximized(!w.is_maximized()));
            }
        });
        let weak = $component.as_weak();
        ops.on_minimize(move || {
            if let Some(c) = weak.upgrade() {
                c.window().with_winit_window(|w| w.set_minimized(true));
            }
        });
        let weak = $component.as_weak();
        ops.on_resize(move |edge| {
            if let Some(c) = weak.upgrade() {
                c.window().with_winit_window(|w| {
                    let _ = w.drag_resize_window($crate::chrome::resize_direction(edge));
                });
            }
        });
        let weak = $component.as_weak();
        let cursor: $crate::chrome::Cursor = $cursor.clone();
        ops.on_show_menu(move || {
            if let Some(c) = weak.upgrade() {
                c.window().with_winit_window(|w| w.show_window_menu(cursor.get()));
            }
        });
        ops.on_close($close);
    }};
}
pub(crate) use install;

/// Keeps `WindowOps.maximized/active` and the cursor position current.
/// Call from the window's winit event filter.
macro_rules! observe {
    ($component:expr, $cursor:expr, $event:expr) => {{
        use slint::ComponentHandle as _;
        use slint::winit_030::WinitWindowAccessor as _;
        match $event {
            slint::winit_030::winit::event::WindowEvent::CursorMoved { position, .. } => $cursor.set(*position),
            slint::winit_030::winit::event::WindowEvent::Focused(active) => {
                $component.global::<$crate::WindowOps>().set_active(*active)
            }
            slint::winit_030::winit::event::WindowEvent::Resized(_) => {
                let maximized = $component.window().with_winit_window(|w| w.is_maximized()).unwrap_or(false);
                $component.global::<$crate::WindowOps>().set_maximized(maximized);
            }
            _ => {}
        }
    }};
}
pub(crate) use observe;

/// Applies a [`Look`] to a component's `Theme` and `WindowOps`.
macro_rules! apply {
    ($component:expr, $look:expr) => {{
        use slint::ComponentHandle as _;
        let look: &$crate::chrome::Look = $look;
        let theme = $component.global::<$crate::Theme>();
        if std::env::var_os("OKNO_COLOR_SCHEME").is_none() {
            if let Some(dark) = look.dark {
                theme.set_dark(dark);
            }
        }
        if let Some(accent) = look.accent {
            theme.set_accent(accent);
        }
        if let Some(layout) = &look.button_layout {
            let (start, end) = $crate::chrome::parse_button_layout(layout);
            let to_model = |v: Vec<String>| {
                slint::ModelRc::new(slint::VecModel::from(
                    v.into_iter().map(slint::SharedString::from).collect::<Vec<_>>(),
                ))
            };
            let ops = $component.global::<$crate::WindowOps>();
            ops.set_start_buttons(to_model(start));
            ops.set_end_buttons(to_model(end));
        }
    }};
}
pub(crate) use apply;

#[cfg(target_os = "linux")]
mod portal {
    use ashpd::desktop::settings::{ColorScheme, Settings};
    use futures_util::StreamExt;

    use super::Look;

    const APPEARANCE: &str = "org.freedesktop.appearance";
    const WM: &str = "org.gnome.desktop.wm.preferences";

    async fn read(settings: &Settings) -> Look {
        let dark = match settings.color_scheme().await {
            Ok(ColorScheme::PreferDark) => Some(true),
            Ok(ColorScheme::PreferLight) => Some(false),
            _ => None,
        };
        let accent = settings.accent_color().await.ok().and_then(|c| {
            let ok = |v: f64| (0.0..=1.0).contains(&v);
            (ok(c.red()) && ok(c.green()) && ok(c.blue())).then(|| {
                slint::Color::from_rgb_u8(
                    (c.red() * 255.0).round() as u8,
                    (c.green() * 255.0).round() as u8,
                    (c.blue() * 255.0).round() as u8,
                )
            })
        });
        let button_layout = settings.read::<String>(WM, "button-layout").await.ok();
        Look { dark, accent, button_layout }
    }

    /// Reads the look, then calls `changed` again after every relevant
    /// setting change. Runs until the portal goes away.
    pub async fn watch(changed: impl Fn(Look)) {
        let Ok(settings) = Settings::new().await else { return };
        changed(read(&settings).await);
        let Ok(mut stream) = settings.receive_setting_changed().await else { return };
        while let Some(setting) = stream.next().await {
            if setting.namespace() == APPEARANCE || (setting.namespace() == WM && setting.key() == "button-layout") {
                changed(read(&settings).await);
            }
        }
    }
}

/// Follows the desktop look; `changed` is called on the UI thread.
pub fn watch_look(rt: &tokio::runtime::Handle, changed: impl Fn(Look) + Send + Sync + 'static) {
    #[cfg(target_os = "linux")]
    {
        let changed = std::sync::Arc::new(changed);
        rt.spawn(portal::watch(move |look| {
            let changed = changed.clone();
            let _ = slint::invoke_from_event_loop(move || changed(look));
        }));
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rt, changed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_gnome_layouts() {
        assert_eq!(parse_button_layout("appmenu:close"), (v(&[]), v(&["close"])));
        assert_eq!(parse_button_layout(":minimize,maximize,close"), (v(&[]), v(&["minimize", "maximize", "close"])));
        assert_eq!(parse_button_layout("close,minimize:appmenu"), (v(&["close", "minimize"]), v(&[])));
        assert_eq!(parse_button_layout("close"), (v(&[]), v(&["close"])));
    }
}
