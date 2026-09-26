use std::sync::{Condvar, Mutex};
use std::time::Duration;

use okno_codec::RawFrame;

/// Single-frame mailbox: the producer overwrites, the consumer takes the
/// newest.
#[derive(Default)]
pub struct FrameSlot {
    state: Mutex<State>,
    ready: Condvar,
}

#[derive(Default)]
struct State {
    frame: Option<RawFrame>,
    closed: bool,
}

#[derive(Debug)]
pub enum Taken {
    Frame(RawFrame),
    Timeout,
    Closed,
}

impl FrameSlot {
    pub fn put(&self, frame: RawFrame) {
        self.state.lock().unwrap().frame = Some(frame);
        self.ready.notify_one();
    }

    /// Marks the source as finished (display gone, session revoked).
    pub fn close(&self) {
        self.state.lock().unwrap().closed = true;
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().unwrap().closed
    }

    /// Waits up to `timeout` for a frame newer than the last one taken.
    pub fn take(&self, timeout: Duration) -> Taken {
        let guard = self.state.lock().unwrap();
        let (mut state, _) = self.ready.wait_timeout_while(guard, timeout, |s| s.frame.is_none() && !s.closed).unwrap();
        match state.frame.take() {
            Some(frame) => Taken::Frame(frame),
            None if state.closed => Taken::Closed,
            None => Taken::Timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use okno_codec::PixelFormat;

    use super::*;

    fn frame(w: u32) -> RawFrame {
        RawFrame::packed(w, 1, PixelFormat::Bgra, vec![0; w as usize * 4])
    }

    #[test]
    fn keeps_only_newest() {
        let slot = FrameSlot::default();
        slot.put(frame(1));
        slot.put(frame(2));
        assert!(matches!(slot.take(Duration::ZERO), Taken::Frame(f) if f.width == 2));
        assert!(matches!(slot.take(Duration::from_millis(10)), Taken::Timeout));
    }

    #[test]
    fn wakes_waiter_and_reports_close() {
        let slot = Arc::new(FrameSlot::default());
        let producer = slot.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            producer.put(frame(3));
            producer.close();
        });
        assert!(matches!(slot.take(Duration::from_secs(5)), Taken::Frame(f) if f.width == 3));
        assert!(matches!(slot.take(Duration::from_secs(5)), Taken::Closed));
    }
}
