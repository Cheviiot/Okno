use std::collections::VecDeque;
use std::sync::Mutex;

use crate::{CHANNELS, RATE};

/// Jitter buffer between the network and the audio device.
///
/// Playback starts once `target` is buffered, so small network hiccups do
/// not cause gaps; if the buffer grows past `max` (the device is slower than
/// the sender, or packets bunched up) the oldest audio is dropped to keep
/// latency bounded.
pub struct SampleRing {
    state: Mutex<State>,
    target: usize,
    max: usize,
}

struct State {
    samples: VecDeque<f32>,
    playing: bool,
}

impl Default for SampleRing {
    /// 60 ms target, 200 ms maximum.
    fn default() -> Self {
        Self::new(60, 200)
    }
}

impl SampleRing {
    pub fn new(target_ms: usize, max_ms: usize) -> Self {
        let per_ms = RATE as usize * CHANNELS / 1000;
        Self {
            state: Mutex::new(State { samples: VecDeque::new(), playing: false }),
            target: target_ms * per_ms,
            max: max_ms * per_ms,
        }
    }

    pub fn push(&self, samples: &[f32]) {
        let mut s = self.state.lock().unwrap();
        s.samples.extend(samples);
        let len = s.samples.len();
        if len > self.max {
            // Keep whole sample pairs so channels stay aligned.
            let excess = (len - self.target) / CHANNELS * CHANNELS;
            s.samples.drain(..excess);
        }
    }

    /// Fills `out` for the device; silence while buffering or on underrun.
    pub fn pull(&self, out: &mut [f32]) {
        let mut s = self.state.lock().unwrap();
        if !s.playing && s.samples.len() >= self.target {
            s.playing = true;
        }
        if !s.playing {
            out.fill(0.0);
            return;
        }
        let n = out.len().min(s.samples.len());
        for (o, v) in out.iter_mut().zip(s.samples.drain(..n)) {
            *o = v;
        }
        if n < out.len() {
            out[n..].fill(0.0);
            // Underrun: build up the buffer again before resuming.
            s.playing = false;
        }
    }

    pub fn buffered(&self) -> usize {
        self.state.lock().unwrap().samples.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_target_then_plays() {
        let ring = SampleRing::new(10, 100);
        let per_ms = RATE as usize * CHANNELS / 1000;
        let mut out = vec![1.0; 4];
        ring.push(&vec![0.5; 5 * per_ms]);
        ring.pull(&mut out);
        assert_eq!(out, [0.0; 4]);
        ring.push(&vec![0.5; 5 * per_ms]);
        ring.pull(&mut out);
        assert_eq!(out, [0.5; 4]);
    }

    #[test]
    fn caps_latency() {
        let ring = SampleRing::new(10, 50);
        let per_ms = RATE as usize * CHANNELS / 1000;
        ring.push(&vec![0.1; 80 * per_ms]);
        assert!(ring.buffered() <= 50 * per_ms);
        assert_eq!(ring.buffered() % CHANNELS, 0);
    }

    #[test]
    fn underrun_pads_with_silence() {
        let ring = SampleRing::new(0, 100);
        ring.push(&[0.5, 0.5]);
        let mut out = vec![1.0; 6];
        ring.pull(&mut out);
        assert_eq!(out, [0.5, 0.5, 0.0, 0.0, 0.0, 0.0]);
    }
}
