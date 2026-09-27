//! Virtual screens on Windows through the Virtual Display Driver
//! (<https://github.com/VirtualDrivers/Virtual-Display-Driver>), a signed
//! indirect display driver the user installs once. It adds a monitor that
//! offers resolutions the real screens do not have.
//!
//! Okno only rearranges screens with the ordinary display settings API: for
//! a session the virtual monitor is attached as the primary screen and the
//! real ones are detached; afterwards the saved layout is put back. The
//! layout is also written to a file first, so a crash is repaired on the
//! next start.

use std::path::PathBuf;

use windows::Win32::Foundation::POINTL;
use windows::Win32::Graphics::Gdi::{
    CDS_NORESET, CDS_SET_PRIMARY, CDS_TYPE, CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW,
    DISP_CHANGE_SUCCESSFUL, DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_PRIMARY_DEVICE, DISPLAY_DEVICEW,
    DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH, DM_POSITION, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE,
    EnumDisplayDevicesW, EnumDisplaySettingsW,
};
use windows::core::PCWSTR;

/// Smallest mode worth offering.
const MIN_WIDTH: u32 = 1280;

/// A display adapter output, such as `\\.\DISPLAY1`.
type DeviceName = [u16; 32];

fn text(wide: &[u16]) -> String {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

fn device_name(name: &str) -> DeviceName {
    let mut out = [0u16; 32];
    for (dst, src) in out.iter_mut().take(31).zip(name.encode_utf16()) {
        *dst = src;
    }
    out
}

struct Output {
    name: DeviceName,
    description: String,
    attached: bool,
    primary: bool,
}

fn outputs() -> Vec<Output> {
    let mut list = Vec::new();
    for i in 0.. {
        let mut device = DISPLAY_DEVICEW { cb: std::mem::size_of::<DISPLAY_DEVICEW>() as u32, ..Default::default() };
        if !unsafe { EnumDisplayDevicesW(PCWSTR::null(), i, &mut device, 0) }.as_bool() {
            break;
        }
        list.push(Output {
            name: device.DeviceName,
            description: text(&device.DeviceString),
            attached: device.StateFlags.contains(DISPLAY_DEVICE_ATTACHED_TO_DESKTOP),
            primary: device.StateFlags.contains(DISPLAY_DEVICE_PRIMARY_DEVICE),
        });
    }
    list
}

fn is_virtual(output: &Output) -> bool {
    let d = output.description.to_lowercase();
    d.contains("virtual display driver") || d.contains("iddsampledriver") || d.contains("vdd by mtt")
}

/// The driver's output, when it is installed.
pub fn find() -> Option<String> {
    outputs().into_iter().find(is_virtual).map(|o| text(&o.name))
}

fn empty_mode() -> DEVMODEW {
    DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() }
}

/// Resolutions the virtual monitor offers, largest first.
pub fn modes(output: &str) -> Vec<(u32, u32)> {
    let name = device_name(output);
    let mut modes = Vec::new();
    for i in 0.. {
        let mut mode = empty_mode();
        if !unsafe { EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut mode) }.as_bool() {
            break;
        }
        let size = (mode.dmPelsWidth, mode.dmPelsHeight);
        if size.0 >= MIN_WIDTH && !modes.contains(&size) {
            modes.push(size);
        }
    }
    modes.sort_by_key(|&(w, h)| std::cmp::Reverse(w as u64 * h as u64));
    modes
}

/// Where one real screen was, to put it back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Placed {
    name: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    frequency: u32,
    primary: bool,
}

impl Placed {
    fn to_line(&self) -> String {
        let p = self;
        format!("{}\t{}\t{}\t{}\t{}\t{}\t{}", p.name, p.width, p.height, p.x, p.y, p.frequency, p.primary)
    }

    fn from_line(line: &str) -> Option<Self> {
        let f: Vec<&str> = line.split('\t').collect();
        let [name, width, height, x, y, frequency, primary] = f.as_slice() else { return None };
        Some(Self {
            name: (*name).to_owned(),
            width: width.parse().ok()?,
            height: height.parse().ok()?,
            x: x.parse().ok()?,
            y: y.parse().ok()?,
            frequency: frequency.parse().ok()?,
            primary: primary.parse().ok()?,
        })
    }
}

/// Queues a mode change for one output; `size` `None` detaches it.
fn stage(name: &str, size: Option<(u32, u32)>, position: (i32, i32), frequency: u32, primary: bool) -> bool {
    let name = device_name(name);
    let mut mode = empty_mode();
    let (width, height) = size.unwrap_or((0, 0));
    mode.dmPelsWidth = width;
    mode.dmPelsHeight = height;
    mode.dmFields = DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT;
    if frequency > 0 && size.is_some() {
        mode.dmDisplayFrequency = frequency;
        mode.dmFields |= DM_DISPLAYFREQUENCY;
    }
    // Display devices use the position member of this union.
    mode.Anonymous1.Anonymous2.dmPosition = POINTL { x: position.0, y: position.1 };
    let mut flags = CDS_UPDATEREGISTRY | CDS_NORESET;
    if primary {
        flags |= CDS_SET_PRIMARY;
    }
    let result = unsafe { ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&mode), None, flags, None) };
    if result != DISP_CHANGE_SUCCESSFUL {
        tracing::warn!("display change for {} refused: {result:?}", text(&name));
        return false;
    }
    true
}

/// Applies the staged changes at once.
fn apply() -> bool {
    let result = unsafe { ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None) };
    result == DISP_CHANGE_SUCCESSFUL
}

fn current_layout(skip: &str) -> Vec<Placed> {
    outputs()
        .into_iter()
        .filter(|o| o.attached && text(&o.name) != skip)
        .filter_map(|o| {
            let mut mode = empty_mode();
            let ok = unsafe { EnumDisplaySettingsW(PCWSTR(o.name.as_ptr()), ENUM_CURRENT_SETTINGS, &mut mode) };
            if !ok.as_bool() {
                return None;
            }
            // SAFETY: display devices use the position member of this union.
            let position = unsafe { mode.Anonymous1.Anonymous2.dmPosition };
            Some(Placed {
                name: text(&o.name),
                width: mode.dmPelsWidth,
                height: mode.dmPelsHeight,
                x: position.x,
                y: position.y,
                frequency: mode.dmDisplayFrequency,
                primary: o.primary,
            })
        })
        .collect()
}

fn recovery_file() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Okno").join("virtual-display-restore.txt"))
}

fn save_recovery(virtual_output: &str, layout: &[Placed]) {
    let Some(path) = recovery_file() else { return };
    let mut body = format!("{virtual_output}\n");
    for placed in layout {
        body.push_str(&placed.to_line());
        body.push('\n');
    }
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(&path));
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!("cannot save the screen layout to {}: {e}", path.display());
    }
}

fn restore_layout(virtual_output: &str, layout: &[Placed]) {
    let mut ok = true;
    for p in layout {
        ok &= stage(&p.name, Some((p.width, p.height)), (p.x, p.y), p.frequency, p.primary);
    }
    ok &= stage(virtual_output, None, (0, 0), 0, false);
    if !(ok && apply()) {
        tracing::warn!("the previous screen layout could not be fully restored");
    }
    if let Some(path) = recovery_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// Puts back a layout left behind by a session that did not end cleanly.
pub fn recover() {
    let Some(path) = recovery_file() else { return };
    let Ok(body) = std::fs::read_to_string(&path) else { return };
    let mut lines = body.lines();
    let Some(virtual_output) = lines.next() else { return };
    let layout: Vec<Placed> = lines.filter_map(Placed::from_line).collect();
    if layout.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    tracing::info!("restoring the screen layout from an interrupted session");
    restore_layout(virtual_output, &layout);
}

/// The virtual screen is active while this lives.
pub struct Active {
    output: String,
    saved: Vec<Placed>,
}

impl Drop for Active {
    fn drop(&mut self) {
        restore_layout(&self.output, &self.saved);
    }
}

/// Makes the virtual monitor the only screen, at `width`×`height`.
pub fn activate(output: &str, width: u32, height: u32) -> Result<Active, String> {
    let saved = current_layout(output);
    if saved.is_empty() {
        return Err("no screen is attached".into());
    }
    save_recovery(output, &saved);
    let active = Active { output: output.to_owned(), saved };
    let mut ok = stage(output, Some((width, height)), (0, 0), 0, true);
    for p in &active.saved {
        ok &= stage(&p.name, None, (0, 0), 0, false);
    }
    // On failure dropping `active` puts the old layout back.
    if !(ok && apply()) {
        return Err(format!("Windows refused to switch to a {width}x{height} virtual screen"));
    }
    Ok(active)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_lines_round_trip() {
        let p = Placed {
            name: r"\\.\DISPLAY1".into(),
            width: 1920,
            height: 1080,
            x: -1920,
            y: 0,
            frequency: 60,
            primary: true,
        };
        assert_eq!(Placed::from_line(&p.to_line()), Some(p));
        assert_eq!(Placed::from_line("broken"), None);
    }
}
