use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okno_codec::{PixelFormat, RawFrame};
use okno_proto::InputEvent;

use tokio::sync::{mpsc, watch};

use crate::{Capture, ClipboardLink, Desktop, DesktopError, DisplayInfo, FrameSlot, VirtualDisplay};

/// Input events received by a [`TestDesktop`].
pub type TestInputLog = Arc<Mutex<Vec<InputEvent>>>;

/// A fake desktop: one display showing a moving colour pattern, and an input
/// log instead of real injection. For tests and `okno-cli host --test`.
pub struct TestDesktop {
    width: u32,
    height: u32,
    fps: u32,
    inputs: TestInputLog,
    copied: watch::Sender<Option<Arc<str>>>,
    pasted: Arc<Mutex<Vec<String>>>,
    paste: mpsc::UnboundedSender<String>,
    /// Size of the virtual screen, when one is active.
    virtual_screen: Arc<Mutex<Option<(u32, u32)>>>,
    virtual_supported: bool,
}

/// Id of the test desktop's virtual screen.
const VIRTUAL_ID: u32 = 1;

/// Clears the virtual screen when the session drops it.
struct EndVirtual(Arc<Mutex<Option<(u32, u32)>>>);

impl Drop for EndVirtual {
    fn drop(&mut self) {
        *self.0.lock().unwrap() = None;
    }
}

impl TestDesktop {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        let (copied, _) = watch::channel(None);
        let pasted = Arc::new(Mutex::new(Vec::new()));
        let (paste, mut requests) = mpsc::unbounded_channel::<String>();
        let log = pasted.clone();
        std::thread::spawn(move || {
            while let Some(text) = requests.blocking_recv() {
                tracing::info!("test desktop clipboard set: {text:?}");
                log.lock().unwrap().push(text);
            }
        });
        Self {
            width,
            height,
            fps: fps.max(1),
            inputs: TestInputLog::default(),
            copied,
            pasted,
            paste,
            virtual_screen: Arc::default(),
            virtual_supported: false,
        }
    }

    /// Offers virtual screens (1920×1080 and 2560×1440) that replace the
    /// test pattern display while in use.
    pub fn with_virtual_display(mut self) -> Self {
        self.virtual_supported = true;
        self
    }

    pub fn inputs(&self) -> TestInputLog {
        self.inputs.clone()
    }

    /// Simulates the host user copying text.
    pub fn copy(&self, text: &str) {
        self.copied.send_replace(Some(Arc::from(text)));
    }

    /// Texts the remote side put on this clipboard.
    pub fn pasted(&self) -> Arc<Mutex<Vec<String>>> {
        self.pasted.clone()
    }
}

/// Colour of pixel (x, y) in frame `n`: a diagonal gradient that scrolls.
pub fn pattern_pixel(x: u32, y: u32, n: u32) -> [u8; 4] {
    let v = x.wrapping_add(y).wrapping_add(n * 4);
    [(v & 0xFF) as u8, ((x * 255) / 640).min(255) as u8, ((y * 255) / 480).min(255) as u8, 255]
}

struct StopOnDrop(Arc<AtomicBool>);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

impl Desktop for TestDesktop {
    fn displays(&self) -> Vec<DisplayInfo> {
        if let Some((width, height)) = *self.virtual_screen.lock().unwrap() {
            return vec![DisplayInfo { id: VIRTUAL_ID, name: "Virtual".into(), width, height, primary: true }];
        }
        vec![DisplayInfo { id: 0, name: "Test pattern".into(), width: self.width, height: self.height, primary: true }]
    }

    fn virtual_modes(&self) -> Vec<(u32, u32)> {
        if self.virtual_supported { vec![(2560, 1440), (1920, 1080)] } else { Vec::new() }
    }

    fn virtual_display(&self, width: u32, height: u32) -> Result<VirtualDisplay, DesktopError> {
        if !self.virtual_modes().contains(&(width, height)) {
            return Err(DesktopError::Unsupported(format!("virtual display {width}x{height}")));
        }
        *self.virtual_screen.lock().unwrap() = Some((width, height));
        let display = DisplayInfo { id: VIRTUAL_ID, name: "Virtual".into(), width, height, primary: true };
        Ok(VirtualDisplay::new(display, EndVirtual(self.virtual_screen.clone())))
    }

    fn capture(&self, display: u32) -> Result<Capture, DesktopError> {
        let (w, h) = match (display, *self.virtual_screen.lock().unwrap()) {
            (0, None) => (self.width, self.height),
            (VIRTUAL_ID, Some(size)) => size,
            _ => return Err(DesktopError::NoDisplay(display)),
        };
        let slot = Arc::new(FrameSlot::default());
        let stop = Arc::new(AtomicBool::new(false));
        let fps = self.fps;
        let producer = slot.clone();
        let stopped = stop.clone();
        std::thread::Builder::new()
            .name("okno-test-capture".into())
            .spawn(move || {
                let mut n = 0u32;
                while !stopped.load(Ordering::Relaxed) {
                    let mut data = Vec::with_capacity((w * h * 4) as usize);
                    for y in 0..h {
                        for x in 0..w {
                            data.extend_from_slice(&pattern_pixel(x, y, n));
                        }
                    }
                    producer.put(RawFrame::packed(w, h, PixelFormat::Bgra, data));
                    n = n.wrapping_add(1);
                    std::thread::sleep(Duration::from_secs(1) / fps);
                }
                producer.close();
            })
            .map_err(|e| DesktopError::Capture(e.to_string()))?;
        Ok(Capture::new(slot, StopOnDrop(stop)))
    }

    fn clipboard(&self) -> Option<ClipboardLink> {
        Some(ClipboardLink { copied: self.copied.subscribe(), paste: self.paste.clone() })
    }

    fn inject(&self, event: InputEvent) {
        tracing::info!("test desktop input: {:?}", event.event);
        self.inputs.lock().unwrap().push(event);
    }
}
