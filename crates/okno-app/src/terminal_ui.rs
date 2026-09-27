//! Window of a remote shell: vt100 emulation rendered as runs of text.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use okno_core::terminal::{RemoteTerminal, TerminalEvent, Terminals};
use slint::{Color, ComponentHandle, ModelRc, VecModel};

use crate::chrome::{self, Cursor, Look};
use crate::clipboard::LocalClipboard;
use crate::{Messages, TermRun, TerminalWindow};

const SCROLLBACK: usize = 5000;

/// GNOME Console / libadwaita palette for the 16 ANSI colours.
const PALETTE: [(u8, u8, u8); 16] = [
    (0x24, 0x1f, 0x31),
    (0xc0, 0x1c, 0x28),
    (0x2e, 0xc2, 0x7e),
    (0xf5, 0xc2, 0x11),
    (0x1e, 0x78, 0xe4),
    (0x98, 0x41, 0xbb),
    (0x0a, 0xb9, 0xdc),
    (0xc0, 0xbf, 0xbc),
    (0x5e, 0x5c, 0x64),
    (0xed, 0x33, 0x3b),
    (0x57, 0xe3, 0x89),
    (0xf8, 0xe4, 0x5c),
    (0x51, 0xa1, 0xff),
    (0xc0, 0x61, 0xcb),
    (0x4f, 0xd2, 0xfd),
    (0xf6, 0xf5, 0xf4),
];

fn indexed(i: u8) -> Color {
    let (r, g, b) = match i {
        0..=15 => PALETTE[i as usize],
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    };
    Color::from_rgb_u8(r, g, b)
}

fn color(c: vt100::Color) -> Option<Color> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(indexed(i)),
        vt100::Color::Rgb(r, g, b) => Some(Color::from_rgb_u8(r, g, b)),
    }
}

#[derive(PartialEq)]
struct Style {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    italic: bool,
    underline: bool,
}

fn style(cell: &vt100::Cell, default_fg: Color, default_bg: Color) -> Style {
    let (mut fg, mut bg) = (color(cell.fgcolor()), color(cell.bgcolor()));
    if cell.inverse() {
        (fg, bg) = (Some(bg.unwrap_or(default_bg)), Some(fg.unwrap_or(default_fg)));
    }
    Style { fg, bg, bold: cell.bold(), italic: cell.italic(), underline: cell.underline() }
}

/// Runs of cells sharing a style; blank runs without background are skipped.
fn runs(screen: &vt100::Screen, default_fg: Color, default_bg: Color) -> Vec<TermRun> {
    let (rows, cols) = screen.size();
    let mut out = Vec::new();
    for row in 0..rows {
        let mut col = 0;
        while col < cols {
            let Some(first) = screen.cell(row, col) else { break };
            let st = style(first, default_fg, default_bg);
            let start = col;
            let mut text = String::new();
            while col < cols {
                let Some(cell) = screen.cell(row, col) else { break };
                if style(cell, default_fg, default_bg) != st {
                    break;
                }
                if !cell.is_wide_continuation() {
                    text.push_str(if cell.has_contents() { cell.contents() } else { " " });
                }
                col += 1;
            }
            if st.bg.is_none() && !st.underline && text.trim().is_empty() {
                continue;
            }
            out.push(TermRun {
                row: row as i32,
                col: start as i32,
                len: (col - start) as i32,
                text: text.into(),
                fg: st.fg.unwrap_or(default_fg),
                default_fg: st.fg.is_none(),
                bg: st.bg.unwrap_or_default(),
                has_bg: st.bg.is_some(),
                bold: st.bold,
                italic: st.italic,
                underline: st.underline,
            });
        }
    }
    out
}

/// Bytes a key sends to the shell (xterm conventions).
fn key_bytes(text: &str, ctrl: bool, alt: bool, app_cursor: bool) -> Option<Vec<u8>> {
    let arrow = |c: char| {
        if app_cursor { format!("\x1bO{c}") } else { format!("\x1b[{c}") }
    };
    let special = match text {
        "\n" => Some("\r".to_owned()),
        "\u{8}" => Some("\x7f".to_owned()),
        "\u{7f}" => Some("\x1b[3~".to_owned()),
        "\u{19}" => Some("\x1b[Z".to_owned()),
        "\u{F700}" => Some(arrow('A')),
        "\u{F701}" => Some(arrow('B')),
        "\u{F702}" => Some(arrow('D')),
        "\u{F703}" => Some(arrow('C')),
        "\u{F729}" => Some("\x1b[H".to_owned()),
        "\u{F72B}" => Some("\x1b[F".to_owned()),
        "\u{F72C}" => Some("\x1b[5~".to_owned()),
        "\u{F72D}" => Some("\x1b[6~".to_owned()),
        "\u{F727}" => Some("\x1b[2~".to_owned()),
        "\u{F704}" => Some("\x1bOP".to_owned()),
        "\u{F705}" => Some("\x1bOQ".to_owned()),
        "\u{F706}" => Some("\x1bOR".to_owned()),
        "\u{F707}" => Some("\x1bOS".to_owned()),
        "\u{F708}" => Some("\x1b[15~".to_owned()),
        "\u{F709}" => Some("\x1b[17~".to_owned()),
        "\u{F70A}" => Some("\x1b[18~".to_owned()),
        "\u{F70B}" => Some("\x1b[19~".to_owned()),
        "\u{F70C}" => Some("\x1b[20~".to_owned()),
        "\u{F70D}" => Some("\x1b[21~".to_owned()),
        "\u{F70E}" => Some("\x1b[23~".to_owned()),
        "\u{F70F}" => Some("\x1b[24~".to_owned()),
        _ => None,
    };
    let mut bytes = match special {
        Some(s) => s.into_bytes(),
        None => {
            let mut chars = text.chars();
            let c = chars.next()?;
            // Other private-use codes are keys a terminal ignores
            // (modifiers alone, media keys).
            if ('\u{F700}'..='\u{F8FF}').contains(&c) {
                return None;
            }
            if ctrl && chars.next().is_none() {
                match c.to_ascii_lowercase() {
                    l @ 'a'..='z' => vec![l as u8 - b'a' + 1],
                    ' ' | '2' | '@' => vec![0],
                    '[' | '3' => vec![0x1b],
                    '\\' | '4' => vec![0x1c],
                    ']' | '5' => vec![0x1d],
                    '6' => vec![0x1e],
                    '/' | '7' | '-' => vec![0x1f],
                    _ => text.as_bytes().to_vec(),
                }
            } else {
                text.as_bytes().to_vec()
            }
        }
    };
    if alt {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

pub struct TerminalView {
    window: TerminalWindow,
    terminal: Rc<RefCell<Option<RemoteTerminal>>>,
    _timer: slint::Timer,
}

impl TerminalView {
    pub fn open(
        terminals: &Terminals,
        host: &str,
        look: &Look,
        clipboard: LocalClipboard,
    ) -> Result<Rc<Self>, slint::PlatformError> {
        let window = TerminalWindow::new()?;
        chrome::apply!(window, look);
        let cursor = Cursor::default();
        window.set_term_title(window.global::<Messages>().invoke_terminal_title(host.into()));

        let parser = Rc::new(RefCell::new(vt100::Parser::new(24, 80, SCROLLBACK)));
        let (terminal, mut events) = terminals.open(80, 24);
        let terminal = Rc::new(RefCell::new(Some(terminal)));
        let dirty = Rc::new(Cell::new(true));

        // Shell output.
        {
            let parser = parser.clone();
            let dirty = dirty.clone();
            let weak = window.as_weak();
            let _ = slint::spawn_local(async move {
                while let Some(event) = events.recv().await {
                    match event {
                        TerminalEvent::Output(bytes) => {
                            parser.borrow_mut().process(&bytes);
                            dirty.set(true);
                        }
                        TerminalEvent::Exited(code) => {
                            if let Some(w) = weak.upgrade() {
                                w.set_status(w.global::<Messages>().invoke_terminal_exited(code));
                                w.set_exited(true);
                            }
                            break;
                        }
                    }
                }
            });
        }

        // Repaint at most once per frame.
        let timer = slint::Timer::default();
        {
            let parser = parser.clone();
            let dirty = dirty.clone();
            let weak = window.as_weak();
            timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
                if !dirty.replace(false) {
                    return;
                }
                let Some(w) = weak.upgrade() else { return };
                let theme = w.global::<crate::Theme>();
                let dark = theme.get_dark();
                let fg = theme.get_fg();
                let bg = if dark { Color::from_rgb_u8(0x1e, 0x1e, 0x1e) } else { Color::from_rgb_u8(0xff, 0xff, 0xff) };
                let parser = parser.borrow();
                let screen = parser.screen();
                w.set_runs(ModelRc::new(VecModel::from(runs(screen, fg, bg))));
                let (row, col) = screen.cursor_position();
                w.set_cursor_row(row as i32);
                w.set_cursor_col(col as i32);
                w.set_cursor_visible(!screen.hide_cursor() && screen.scrollback() == 0);
            });
        }

        {
            let parser = parser.clone();
            let terminal = terminal.clone();
            let dirty = dirty.clone();
            window.on_resized(move |cols, rows| {
                let (cols, rows) = (cols.clamp(2, 1000) as u16, rows.clamp(2, 500) as u16);
                if parser.borrow().screen().size() == (rows, cols) {
                    return;
                }
                parser.borrow_mut().screen_mut().set_size(rows, cols);
                if let Some(t) = terminal.borrow().as_ref() {
                    t.resize(cols as u32, rows as u32);
                }
                dirty.set(true);
            });
        }
        {
            let parser = parser.clone();
            let terminal = terminal.clone();
            let clipboard = clipboard.clone();
            let dirty = dirty.clone();
            window.on_key(move |text, ctrl, alt, shift| {
                let term = terminal.borrow();
                let Some(t) = term.as_ref() else { return };
                // Ctrl+Shift+V pastes, as in GNOME Console.
                if ctrl && shift && text.eq_ignore_ascii_case("v") {
                    if let Some(paste) = clipboard.get() {
                        let bracketed = parser.borrow().screen().bracketed_paste();
                        let mut bytes = Vec::new();
                        if bracketed {
                            bytes.extend_from_slice(b"\x1b[200~");
                        }
                        bytes.extend_from_slice(paste.replace("\r\n", "\r").replace('\n', "\r").as_bytes());
                        if bracketed {
                            bytes.extend_from_slice(b"\x1b[201~");
                        }
                        t.input(bytes);
                    }
                    return;
                }
                let app_cursor = parser.borrow().screen().application_cursor();
                if let Some(bytes) = key_bytes(&text, ctrl, alt, app_cursor) {
                    if parser.borrow().screen().scrollback() != 0 {
                        parser.borrow_mut().screen_mut().set_scrollback(0);
                        dirty.set(true);
                    }
                    t.input(bytes);
                }
            });
        }
        {
            let parser = parser.clone();
            let dirty = dirty.clone();
            window.on_scrolled(move |lines| {
                let mut p = parser.borrow_mut();
                let current = p.screen().scrollback() as i32;
                p.screen_mut().set_scrollback((current + lines).max(0) as usize);
                dirty.set(true);
            });
        }

        {
            let terminal = terminal.clone();
            let weak = window.as_weak();
            chrome::install!(window, cursor, move || {
                terminal.borrow_mut().take();
                if let Some(w) = weak.upgrade() {
                    let _ = w.hide();
                }
            });
        }
        {
            let terminal = terminal.clone();
            window.window().on_close_requested(move || {
                terminal.borrow_mut().take();
                slint::CloseRequestResponse::HideWindow
            });
        }
        {
            let weak = window.as_weak();
            use slint::winit_030::WinitWindowAccessor as _;
            window.window().on_winit_window_event(move |_, event| {
                if let Some(w) = weak.upgrade() {
                    chrome::observe!(w, cursor, event);
                }
                slint::winit_030::EventResult::Propagate
            });
        }

        window.show()?;
        Ok(Rc::new(Self { window, terminal, _timer: timer }))
    }

    pub fn apply_look(&self, look: &Look) {
        chrome::apply!(self.window, look);
    }

    /// Ends the shell and closes the window (the session is gone).
    pub fn close(&self) {
        self.terminal.borrow_mut().take();
        let _ = self.window.hide();
    }

    pub fn is_open(&self) -> bool {
        self.terminal.borrow().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_to_bytes() {
        assert_eq!(key_bytes("a", false, false, false), Some(b"a".to_vec()));
        assert_eq!(key_bytes("\n", false, false, false), Some(b"\r".to_vec()));
        assert_eq!(key_bytes("c", true, false, false), Some(vec![3]));
        assert_eq!(key_bytes("x", false, true, false), Some(b"\x1bx".to_vec()));
        assert_eq!(key_bytes("\u{F700}", false, false, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(key_bytes("\u{F700}", false, false, true), Some(b"\x1bOA".to_vec()));
        assert_eq!(key_bytes("\u{8}", false, false, false), Some(b"\x7f".to_vec()));
        assert_eq!(key_bytes("\u{F76A}", false, false, false), None);
        assert_eq!(key_bytes("я", false, false, false), Some("я".as_bytes().to_vec()));
    }

    #[test]
    fn runs_group_styles() {
        let mut p = vt100::Parser::new(2, 20, 0);
        p.process(b"ab\x1b[31mred\x1b[0m  x");
        let fg = Color::from_rgb_u8(255, 255, 255);
        let bg = Color::from_rgb_u8(0, 0, 0);
        let r = runs(p.screen(), fg, bg);
        let texts: Vec<_> = r.iter().map(|r| (r.col, r.text.to_string(), r.default_fg)).collect();
        assert_eq!(texts[0], (0, "ab".into(), true));
        assert_eq!(texts[1], (2, "red".into(), false));
        assert_eq!(r[1].fg, PALETTE[1].into_color());
    }

    trait IntoColor {
        fn into_color(self) -> Color;
    }
    impl IntoColor for (u8, u8, u8) {
        fn into_color(self) -> Color {
            Color::from_rgb_u8(self.0, self.1, self.2)
        }
    }
}
