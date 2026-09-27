//! Access to the local desktop for the host: screen capture, input injection
//! and (later) the clipboard.
//!
//! Capture, input and clipboard share one object because on Linux they share
//! one portal session: the RemoteDesktop portal grants input, the ScreenCast
//! streams are attached to the same session, and the Clipboard portal only
//! works on a RemoteDesktop session.
//!
//! Frames travel through a [`FrameSlot`] that keeps only the newest frame: an
//! encoder that falls behind skips frames instead of queueing them.

pub mod keymap;
mod slot;
mod test_desktop;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod vdd;
#[cfg(windows)]
mod windows;

use std::sync::Arc;

pub use okno_codec::{PixelFormat, RawFrame};
use okno_proto::InputEvent;
pub use slot::{FrameSlot, Taken};
pub use test_desktop::{TestDesktop, TestInputLog};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopError {
    #[error("the user or the system refused screen sharing")]
    Denied,
    #[error("no display with id {0}")]
    NoDisplay(u32),
    #[error("screen capture failed: {0}")]
    Capture(String),
    #[error("desktop access is not supported here: {0}")]
    Unsupported(String),
}

/// A running capture; frames arrive in [`slot`](Self::slot) until dropped.
pub struct Capture {
    pub slot: Arc<FrameSlot>,
    _stop: Box<dyn Send>,
}

impl Capture {
    pub fn new(slot: Arc<FrameSlot>, stop_guard: impl Send + 'static) -> Self {
        Self { slot, _stop: Box::new(stop_guard) }
    }
}

/// A screen that exists only for a session: while this lives the desktop
/// shows on it (other screens may be switched off), and dropping it puts
/// the previous screen layout back.
pub struct VirtualDisplay {
    pub display: DisplayInfo,
    _restore: Box<dyn Send>,
}

impl VirtualDisplay {
    pub fn new(display: DisplayInfo, restore_guard: impl Send + 'static) -> Self {
        Self { display, _restore: Box::new(restore_guard) }
    }
}

/// Largest clipboard text exchanged, in bytes.
pub const MAX_CLIPBOARD: usize = 1024 * 1024;

/// Connection to the host's text clipboard.
#[derive(Clone)]
pub struct ClipboardLink {
    /// Text copied on the host (not text the link itself set).
    pub copied: watch::Receiver<Option<Arc<str>>>,
    /// Puts text on the host clipboard.
    pub paste: mpsc::UnboundedSender<String>,
}

pub trait Desktop: Send + Sync + 'static {
    fn displays(&self) -> Vec<DisplayInfo>;

    /// Starts capturing one display.
    fn capture(&self, display: u32) -> Result<Capture, DesktopError>;

    /// Queues an input event; never blocks. Events for unknown displays or
    /// keys are dropped.
    fn inject(&self, event: InputEvent);

    /// The host clipboard, when the platform grants access.
    fn clipboard(&self) -> Option<ClipboardLink> {
        None
    }

    /// Sizes [`virtual_display`](Self::virtual_display) can create, largest
    /// first; empty when unsupported.
    fn virtual_modes(&self) -> Vec<(u32, u32)> {
        Vec::new()
    }

    /// Moves the desktop to a new virtual screen of `width`×`height`.
    fn virtual_display(&self, width: u32, height: u32) -> Result<VirtualDisplay, DesktopError> {
        let _ = (width, height);
        Err(DesktopError::Unsupported("virtual displays".into()))
    }
}

/// Options for opening the real desktop.
#[derive(Clone, Debug, Default)]
pub struct OpenOptions {
    /// Linux: token from a previous session so the portal does not ask again.
    pub restore_token: Option<String>,
}

pub struct OpenedDesktop {
    pub desktop: Arc<dyn Desktop>,
    /// Linux: token to store for the next [`OpenOptions::restore_token`].
    pub restore_token: Option<String>,
}

/// Opens this machine's desktop. On Linux this may show the portal's
/// permission dialog and waits for the user's answer.
pub async fn open(options: OpenOptions) -> Result<OpenedDesktop, DesktopError> {
    #[cfg(target_os = "linux")]
    {
        linux::PortalDesktop::open(options).await
    }
    #[cfg(windows)]
    {
        let _ = options;
        windows::WinDesktop::open()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = options;
        Err(DesktopError::Unsupported(std::env::consts::OS.into()))
    }
}
