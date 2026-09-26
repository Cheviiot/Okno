use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okno_codec::{PixelFormat, RawFrame};
use okno_proto::InputEvent;

use crate::{Capture, Desktop, DesktopError, DisplayInfo, FrameSlot};

/// Input events received by a [`TestDesktop`].
pub type TestInputLog = Arc<Mutex<Vec<InputEvent>>>;

/// A fake desktop: one display showing a moving colour pattern, and an input
/// log instead of real injection. For tests and `okno-cli host --test`.
pub struct TestDesktop {
    width: u32,
    height: u32,
    fps: u32,
    inputs: TestInputLog,
}

impl TestDesktop {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self { width, height, fps: fps.max(1), inputs: TestInputLog::default() }
    }

    pub fn inputs(&self) -> TestInputLog {
        self.inputs.clone()
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
        vec![DisplayInfo { id: 0, name: "Test pattern".into(), width: self.width, height: self.height, primary: true }]
    }

    fn capture(&self, display: u32) -> Result<Capture, DesktopError> {
        if display != 0 {
            return Err(DesktopError::NoDisplay(display));
        }
        let slot = Arc::new(FrameSlot::default());
        let stop = Arc::new(AtomicBool::new(false));
        let (w, h, fps) = (self.width, self.height, self.fps);
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

    fn inject(&self, event: InputEvent) {
        self.inputs.lock().unwrap().push(event);
    }
}
