//! Windows desktop: Windows.Graphics.Capture for frames, `SendInput` with
//! scan codes for input.
//!
//! The process is made per-monitor DPI aware so monitor rectangles and
//! captured frames are in physical pixels, the same space `SendInput` uses.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use okno_proto::input_event::Event;
use okno_proto::{InputEvent, MouseButton};
use tokio::sync::{mpsc, watch};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO, MONITORINFOEXW};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    SendInput, VIRTUAL_KEY, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, XBUTTON1, XBUTTON2,
};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};

use crate::keymap::{KEY_PAUSE, evdev_to_scancode};
use crate::{
    Capture, ClipboardLink, Desktop, DesktopError, DisplayInfo, FrameSlot, MAX_CLIPBOARD, OpenedDesktop, PixelFormat,
    RawFrame, VirtualDisplay,
};

/// Wheel units per pixel of smooth scrolling (one notch = 120 ≈ 48 px).
const WHEEL_PER_PIXEL: f64 = 2.5;

struct WinMonitor {
    info: DisplayInfo,
    monitor: Monitor,
    rect: RECT,
    /// Output name such as `\\.\DISPLAY1`.
    device: String,
}

// `Monitor` wraps an HMONITOR handle, which is valid from any thread.
unsafe impl Send for WinMonitor {}
unsafe impl Sync for WinMonitor {}

type Monitors = Arc<RwLock<Vec<WinMonitor>>>;

fn enumerate_monitors() -> Result<Vec<WinMonitor>, DesktopError> {
    let monitors = Monitor::enumerate()
        .map_err(|e| DesktopError::Capture(e.to_string()))?
        .into_iter()
        .enumerate()
        .filter_map(|(i, monitor)| {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let ok = unsafe {
                GetMonitorInfoW(
                    HMONITOR(monitor.as_raw_hmonitor()),
                    &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
                )
            };
            if !ok.as_bool() {
                return None;
            }
            let rect = info.monitorInfo.rcMonitor;
            let end = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            Some(WinMonitor {
                info: DisplayInfo {
                    id: i as u32,
                    name: monitor.name().unwrap_or_else(|_| format!("Monitor {}", i + 1)),
                    width: (rect.right - rect.left) as u32,
                    height: (rect.bottom - rect.top) as u32,
                    primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                },
                monitor,
                rect,
                device: String::from_utf16_lossy(&info.szDevice[..end]),
            })
        })
        .collect::<Vec<_>>();
    if monitors.is_empty() {
        return Err(DesktopError::Capture("no monitors found".into()));
    }
    Ok(monitors)
}

fn refresh(monitors: &Monitors) {
    match enumerate_monitors() {
        Ok(list) => *monitors.write().unwrap() = list,
        Err(e) => tracing::warn!("monitor list not updated: {e}"),
    }
}

/// Restores the real screens when the session's virtual screen goes.
struct EndVirtual {
    active: Option<crate::vdd::Active>,
    monitors: Monitors,
}

impl Drop for EndVirtual {
    fn drop(&mut self) {
        drop(self.active.take());
        refresh(&self.monitors);
    }
}

pub struct WinDesktop {
    /// Changes when a virtual screen comes or goes.
    monitors: Monitors,
    /// Serialises `SendInput` so events from one session stay in order.
    input: Mutex<()>,
    clipboard: Option<ClipboardLink>,
}

impl WinDesktop {
    pub fn open() -> Result<OpenedDesktop, DesktopError> {
        unsafe {
            // Fails harmlessly when a manifest already set the awareness.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        // A session that crashed may have left the real screens off.
        crate::vdd::recover();
        let monitors = Arc::new(RwLock::new(enumerate_monitors()?));
        let desktop = Self { monitors, input: Mutex::new(()), clipboard: clipboard_link() };
        Ok(OpenedDesktop { desktop: Arc::new(desktop), restore_token: None })
    }

    fn send(&self, inputs: &[INPUT]) {
        let _guard = self.input.lock().unwrap();
        let sent = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if sent as usize != inputs.len() {
            // Blocked by UIPI (e.g. an elevated window has focus) or the
            // secure desktop is active.
            tracing::debug!("SendInput injected {sent} of {} events", inputs.len());
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

fn keyboard(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } },
    }
}

impl Desktop for WinDesktop {
    fn displays(&self) -> Vec<DisplayInfo> {
        self.monitors.read().unwrap().iter().map(|m| m.info.clone()).collect()
    }

    fn virtual_modes(&self) -> Vec<(u32, u32)> {
        crate::vdd::find().map(|output| crate::vdd::modes(&output)).unwrap_or_default()
    }

    fn virtual_display(&self, width: u32, height: u32) -> Result<VirtualDisplay, DesktopError> {
        let output = crate::vdd::find()
            .ok_or_else(|| DesktopError::Unsupported("the Virtual Display Driver is not installed".into()))?;
        if !crate::vdd::modes(&output).contains(&(width, height)) {
            return Err(DesktopError::Unsupported(format!("the virtual screen has no {width}x{height} mode")));
        }
        let active = crate::vdd::activate(&output, width, height).map_err(DesktopError::Capture)?;
        let end = EndVirtual { active: Some(active), monitors: self.monitors.clone() };
        // Windows needs a moment before the new monitor is enumerable.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            refresh(&self.monitors);
            let found = self.monitors.read().unwrap().iter().find(|m| m.device == output).map(|m| m.info.clone());
            if let Some(info) = found {
                return Ok(VirtualDisplay::new(info, end));
            }
            if std::time::Instant::now() >= deadline {
                // Dropping `end` puts the real screens back.
                return Err(DesktopError::Capture("the virtual screen did not appear".into()));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn capture(&self, display: u32) -> Result<Capture, DesktopError> {
        let monitor =
            self.monitors.read().unwrap().get(display as usize).ok_or(DesktopError::NoDisplay(display))?.monitor;
        let slot = Arc::new(FrameSlot::default());
        let settings = Settings::new(
            monitor,
            CursorCaptureSettings::WithCursor,
            DrawBorderSettings::WithoutBorder,
            SecondaryWindowSettings::Default,
            MinimumUpdateIntervalSettings::Custom(Duration::from_millis(16)),
            DirtyRegionSettings::Default,
            ColorFormat::Bgra8,
            slot.clone(),
        );
        let control = Handler::start_free_threaded(settings).map_err(|e| DesktopError::Capture(e.to_string()))?;
        Ok(Capture::new(slot, StopCapture(Some(control))))
    }

    fn clipboard(&self) -> Option<ClipboardLink> {
        self.clipboard.clone()
    }

    fn inject(&self, event: InputEvent) {
        let Some(event) = event.event else { return };
        match event {
            Event::Motion(m) => {
                let Some(r) = self.monitors.read().unwrap().get(m.display as usize).map(|mon| mon.rect) else { return };
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                    )
                };
                let px = r.left as f64 + m.x.clamp(0.0, 1.0) * (r.right - r.left - 1) as f64;
                let py = r.top as f64 + m.y.clamp(0.0, 1.0) * (r.bottom - r.top - 1) as f64;
                let nx = ((px - vx as f64) * 65535.0 / (vw - 1) as f64).round() as i32;
                let ny = ((py - vy as f64) * 65535.0 / (vh - 1) as f64).round() as i32;
                self.send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
            }
            Event::Button(b) => {
                let (down, up, data) = match MouseButton::try_from(b.button) {
                    Ok(MouseButton::Left) => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
                    Ok(MouseButton::Right) => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
                    Ok(MouseButton::Middle) => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
                    Ok(MouseButton::Back) => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON1 as i32),
                    Ok(MouseButton::Forward) => (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, XBUTTON2 as i32),
                    _ => return,
                };
                self.send(&[mouse(0, 0, data, if b.pressed { down } else { up })]);
            }
            Event::Scroll(s) => {
                // Positive steps scroll down/right; Windows' wheel is
                // positive up, the horizontal wheel positive right.
                let vertical = -(s.steps_y * 120) - (s.dy * WHEEL_PER_PIXEL).round() as i32;
                let horizontal = s.steps_x * 120 + (s.dx * WHEEL_PER_PIXEL).round() as i32;
                let mut inputs = Vec::new();
                if vertical != 0 {
                    inputs.push(mouse(0, 0, vertical, MOUSEEVENTF_WHEEL));
                }
                if horizontal != 0 {
                    inputs.push(mouse(0, 0, horizontal, MOUSEEVENTF_HWHEEL));
                }
                if !inputs.is_empty() {
                    self.send(&inputs);
                }
            }
            Event::Key(k) => {
                let up = if k.pressed { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
                if k.evdev_code == KEY_PAUSE {
                    self.send(&[keyboard(VK_PAUSE, 0, up)]);
                    return;
                }
                let Some(code) = evdev_to_scancode(k.evdev_code) else { return };
                let mut flags = KEYEVENTF_SCANCODE | up;
                if code.extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                self.send(&[keyboard(VIRTUAL_KEY(0), code.code, flags)]);
            }
        }
    }
}

/// Windows has no clipboard change notification without a window, so a
/// thread polls it and applies texts from the remote side.
fn clipboard_link() -> Option<ClipboardLink> {
    let (copied_tx, copied) = watch::channel(None);
    let (paste, mut requests) = mpsc::unbounded_channel::<String>();
    std::thread::Builder::new()
        .name("okno-clipboard".into())
        .spawn(move || {
            let Ok(mut clipboard) = arboard::Clipboard::new() else { return };
            // Existing content is not "copied" in this session.
            let mut last = clipboard.get_text().ok();
            loop {
                loop {
                    match requests.try_recv() {
                        Ok(text) => {
                            let _ = clipboard.set_text(text.clone());
                            last = Some(text);
                        }
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => return,
                    }
                }
                if let Ok(text) = clipboard.get_text() {
                    if last.as_ref() != Some(&text) && text.len() <= MAX_CLIPBOARD {
                        copied_tx.send_replace(Some(Arc::from(text.as_str())));
                        last = Some(text);
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
        .ok()?;
    Some(ClipboardLink { copied, paste })
}

struct Handler {
    slot: Arc<FrameSlot>,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Arc<FrameSlot>;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        Ok(Self { slot: ctx.flags })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, _control: InternalCaptureControl) -> Result<(), Self::Error> {
        let mut buffer = frame.buffer()?;
        let (width, height, stride) = (buffer.width(), buffer.height(), buffer.row_pitch() as usize);
        let raw = buffer.as_raw_buffer();
        let needed = stride * (height as usize).saturating_sub(1) + width as usize * 4;
        if height == 0 || raw.len() < needed {
            return Ok(());
        }
        self.slot.put(RawFrame { width, height, stride, format: PixelFormat::Bgra, data: raw[..needed].to_vec() });
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        self.slot.close();
        Ok(())
    }
}

struct StopCapture(Option<CaptureControl<Handler, Box<dyn std::error::Error + Send + Sync>>>);

impl Drop for StopCapture {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            let _ = control.stop();
        }
    }
}
