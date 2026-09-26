//! Physical key codes. The wire format uses Linux evdev codes (`KEY_*` from
//! `linux/input-event-codes.h`); Windows works with PC/AT set 1 scan codes.

/// A Windows scan code; `extended` means the `E0` prefix
/// (`KEYEVENTF_EXTENDEDKEY`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanCode {
    pub code: u16,
    pub extended: bool,
}

const fn sc(code: u16) -> Option<ScanCode> {
    Some(ScanCode { code, extended: false })
}

const fn ext(code: u16) -> Option<ScanCode> {
    Some(ScanCode { code, extended: true })
}

/// evdev → set 1 scan code. Pause has no single scan code and is injected as
/// a virtual key on Windows, so it maps to `None` here.
pub fn evdev_to_scancode(evdev: u32) -> Option<ScanCode> {
    match evdev {
        // KEY_ESC (1) … KEY_KPDOT (83): evdev codes were taken from set 1.
        1..=83 => sc(evdev as u16),
        86 => sc(0x56),                               // KEY_102ND
        87 => sc(0x57),                               // KEY_F11
        88 => sc(0x58),                               // KEY_F12
        89 => sc(0x73),                               // KEY_RO
        92 => sc(0x79),                               // KEY_HENKAN
        93 => sc(0x70),                               // KEY_KATAKANAHIRAGANA
        94 => sc(0x7B),                               // KEY_MUHENKAN
        96 => ext(0x1C),                              // KEY_KPENTER
        97 => ext(0x1D),                              // KEY_RIGHTCTRL
        98 => ext(0x35),                              // KEY_KPSLASH
        99 => ext(0x37),                              // KEY_SYSRQ (Print Screen)
        100 => ext(0x38),                             // KEY_RIGHTALT
        102 => ext(0x47),                             // KEY_HOME
        103 => ext(0x48),                             // KEY_UP
        104 => ext(0x49),                             // KEY_PAGEUP
        105 => ext(0x4B),                             // KEY_LEFT
        106 => ext(0x4D),                             // KEY_RIGHT
        107 => ext(0x4F),                             // KEY_END
        108 => ext(0x50),                             // KEY_DOWN
        109 => ext(0x51),                             // KEY_PAGEDOWN
        110 => ext(0x52),                             // KEY_INSERT
        111 => ext(0x53),                             // KEY_DELETE
        113 => ext(0x20),                             // KEY_MUTE
        114 => ext(0x2E),                             // KEY_VOLUMEDOWN
        115 => ext(0x30),                             // KEY_VOLUMEUP
        117 => sc(0x59),                              // KEY_KPEQUAL
        122 => sc(0x72),                              // KEY_HANGEUL
        123 => sc(0x71),                              // KEY_HANJA
        124 => sc(0x7D),                              // KEY_YEN
        125 => ext(0x5B),                             // KEY_LEFTMETA
        126 => ext(0x5C),                             // KEY_RIGHTMETA
        127 => ext(0x5D),                             // KEY_COMPOSE (Menu)
        163 => ext(0x19),                             // KEY_NEXTSONG
        164 => ext(0x22),                             // KEY_PLAYPAUSE
        165 => ext(0x10),                             // KEY_PREVIOUSSONG
        166 => ext(0x24),                             // KEY_STOPCD
        183..=193 => sc(0x64 + (evdev - 183) as u16), // KEY_F13 … KEY_F23
        194 => sc(0x76),                              // KEY_F24
        _ => None,
    }
}

/// Scan code → evdev, the inverse of [`evdev_to_scancode`].
pub fn scancode_to_evdev(code: ScanCode) -> Option<u32> {
    // The table is small; a linear search over the evdev range is plenty.
    (1..=255).find(|&e| evdev_to_scancode(e) == Some(code))
}

/// evdev button codes for the pointer.
pub mod button {
    pub const LEFT: i32 = 0x110;
    pub const RIGHT: i32 = 0x111;
    pub const MIDDLE: i32 = 0x112;
    pub const SIDE: i32 = 0x113;
    pub const EXTRA: i32 = 0x114;
}

pub const KEY_PAUSE: u32 = 119;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys() {
        assert_eq!(evdev_to_scancode(1), sc(0x01)); // Esc
        assert_eq!(evdev_to_scancode(30), sc(0x1E)); // A
        assert_eq!(evdev_to_scancode(28), sc(0x1C)); // Enter
        assert_eq!(evdev_to_scancode(96), ext(0x1C)); // keypad Enter
        assert_eq!(evdev_to_scancode(105), ext(0x4B)); // Left
        assert_eq!(evdev_to_scancode(183), sc(0x64)); // F13
        assert_eq!(evdev_to_scancode(193), sc(0x6E)); // F23
        assert_eq!(evdev_to_scancode(194), sc(0x76)); // F24
        assert_eq!(evdev_to_scancode(KEY_PAUSE), None);
    }

    #[test]
    fn inverse_is_consistent() {
        for e in 1..=255 {
            if let Some(code) = evdev_to_scancode(e) {
                assert_eq!(scancode_to_evdev(code), Some(e), "evdev {e}");
            }
        }
    }
}
