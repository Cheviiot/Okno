//! Windows desktop: Windows.Graphics.Capture for frames, `SendInput` with
//! scan codes for input.
//!
//! The process is made per-monitor DPI aware so monitor rectangles and
//! captured frames are in physical pixels, the same space `SendInput` uses.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use okno_proto::input_event::Event;
use okno_proto::{InputEvent, MouseButton};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO};
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
use crate::{Capture, Desktop, DesktopError, DisplayInfo, FrameSlot, OpenedDesktop, PixelFormat, RawFrame};

/// Wheel units per pixel of smooth scrolling (one notch = 120 ≈ 48 px).
const WHEEL_PER_PIXEL: f64 = 2.5;

struct WinMonitor {
    info: DisplayInfo,
    monitor: Monitor,
    rect: RECT,
}

pub struct WinDesktop {
    monitors: Vec<WinMonitor>,
    /// Serialises `SendInput` so events from one session stay in order.
    input: Mutex<()>,
}

// `Monitor` wraps an HMONITOR handle, which is valid from any thread.
unsafe impl Sync for WinDesktop {}

impl WinDesktop {
    pub fn open() -> Result<OpenedDesktop, DesktopError> {
        unsafe {
            // Fails harmlessly when a manifest already set the awareness.
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let monitors = Monitor::enumerate()
            .map_err(|e| DesktopError::Capture(e.to_string()))?
            .into_iter()
            .enumerate()
            .filter_map(|(i, monitor)| {
                let mut info = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
                let ok = unsafe { GetMonitorInfoW(HMONITOR(monitor.as_raw_hmonitor()), &mut info) };
                if !ok.as_bool() {
                    return None;
                }
                let rect = info.rcMonitor;
                Some(WinMonitor {
                    info: DisplayInfo {
                        id: i as u32,
                        name: monitor.name().unwrap_or_else(|_| format!("Monitor {}", i + 1)),
                        width: (rect.right - rect.left) as u32,
                        height: (rect.bottom - rect.top) as u32,
                        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                    },
                    monitor,
                    rect,
                })
            })
            .collect::<Vec<_>>();
        if monitors.is_empty() {
            return Err(DesktopError::Capture("no monitors found".into()));
        }
        Ok(OpenedDesktop { desktop: Arc::new(Self { monitors, input: Mutex::new(()) }), restore_token: None })
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
        self.monitors.iter().map(|m| m.info.clone()).collect()
    }

    fn capture(&self, display: u32) -> Result<Capture, DesktopError> {
        let monitor = self.monitors.get(display as usize).ok_or(DesktopError::NoDisplay(display))?.monitor;
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

    fn inject(&self, event: InputEvent) {
        let Some(event) = event.event else { return };
        match event {
            Event::Motion(m) => {
                let Some(mon) = self.monitors.get(m.display as usize) else { return };
                let (vx, vy, vw, vh) = unsafe {
                    (
                        GetSystemMetrics(SM_XVIRTUALSCREEN),
                        GetSystemMetrics(SM_YVIRTUALSCREEN),
                        GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                        GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                    )
                };
                let r = mon.rect;
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
