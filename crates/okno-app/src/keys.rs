//! Physical keys from winit to the evdev codes of the wire protocol.

use slint::winit_030::winit::keyboard::PhysicalKey;
use slint::winit_030::winit::platform::scancode::PhysicalKeyExtScancode;

/// evdev code of a physical key, if it has one.
pub fn evdev_code(key: PhysicalKey) -> Option<u32> {
    let scancode = key.to_scancode()?;
    #[cfg(windows)]
    {
        // winit reports set 1 scan codes with 0xE000 for extended keys.
        okno_desktop::keymap::scancode_to_evdev(okno_desktop::keymap::ScanCode {
            code: (scancode & 0xFF) as u16,
            extended: scancode & 0xFF00 == 0xE000,
        })
    }
    #[cfg(not(windows))]
    {
        // On X11 and Wayland winit already reports evdev codes.
        Some(scancode)
    }
}
