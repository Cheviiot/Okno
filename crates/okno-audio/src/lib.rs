//! Sound for remote sessions: capture what the host plays, code it with
//! Opus, play it on the client.
//!
//! Everything runs at 48 kHz stereo in 20 ms frames (960 samples per
//! channel), interleaved `f32`.

#[cfg(target_os = "linux")]
mod linux;
mod ring;
#[cfg(windows)]
mod windows;

use std::f32::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub use ring::SampleRing;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
/// Samples per channel in one Opus frame (20 ms).
pub const FRAME: usize = 960;
/// Interleaved samples in one frame.
pub const FRAME_SAMPLES: usize = FRAME * CHANNELS;

#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("Opus: {0}")]
    Opus(#[from] opus::Error),
    #[error("audio device: {0}")]
    Device(String),
}

/// Opus encoder for system sound.
pub struct Encoder {
    inner: opus::Encoder,
    out: Vec<u8>,
}

impl Encoder {
    pub fn new(bitrate: i32) -> Result<Self, AudioError> {
        let mut inner = opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Audio)?;
        inner.set_bitrate(opus::Bitrate::Bits(bitrate))?;
        Ok(Self { inner, out: vec![0; 4000] })
    }

    /// Encodes exactly one frame ([`FRAME_SAMPLES`] samples).
    pub fn encode(&mut self, frame: &[f32]) -> Result<Vec<u8>, AudioError> {
        let n = self.inner.encode_float(frame, &mut self.out)?;
        Ok(self.out[..n].to_vec())
    }
}

pub struct Decoder {
    inner: opus::Decoder,
    out: Vec<f32>,
}

impl Decoder {
    pub fn new() -> Result<Self, AudioError> {
        Ok(Self { inner: opus::Decoder::new(RATE, opus::Channels::Stereo)?, out: vec![0.0; FRAME_SAMPLES * 6] })
    }

    /// Decodes one packet into interleaved samples.
    pub fn decode(&mut self, packet: &[u8]) -> Result<&[f32], AudioError> {
        let per_channel = self.inner.decode_float(packet, &mut self.out, false)?;
        Ok(&self.out[..per_channel * CHANNELS])
    }

    /// Fills a lost packet with concealment.
    pub fn conceal(&mut self) -> Result<&[f32], AudioError> {
        let per_channel = self.inner.decode_float(&[], &mut self.out[..FRAME_SAMPLES], false)?;
        Ok(&self.out[..per_channel * CHANNELS])
    }
}

/// Where the host's sound comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Everything the host plays (the default output's monitor).
    System,
    /// A 440 Hz tone, for tests.
    Tone,
}

/// A running capture; frames stop when dropped.
pub struct Capture {
    stop: Arc<AtomicBool>,
    _platform: Option<Box<dyn Send>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Collects arbitrary-sized sample blocks into whole frames.
pub struct Framer {
    pending: Vec<f32>,
}

impl Default for Framer {
    fn default() -> Self {
        Self { pending: Vec::with_capacity(FRAME_SAMPLES * 2) }
    }
}

impl Framer {
    pub fn push(&mut self, samples: &[f32], mut frame: impl FnMut(&[f32])) {
        self.pending.extend_from_slice(samples);
        let mut start = 0;
        while self.pending.len() - start >= FRAME_SAMPLES {
            frame(&self.pending[start..start + FRAME_SAMPLES]);
            start += FRAME_SAMPLES;
        }
        self.pending.drain(..start);
    }
}

/// Starts capturing; `on_frame` gets one interleaved frame at a time on a
/// capture thread.
pub fn capture(source: Source, mut on_frame: impl FnMut(&[f32]) + Send + 'static) -> Result<Capture, AudioError> {
    let stop = Arc::new(AtomicBool::new(false));
    match source {
        Source::Tone => {
            let stopped = stop.clone();
            std::thread::Builder::new()
                .name("okno-tone".into())
                .spawn(move || {
                    let mut phase = 0f32;
                    let mut frame = vec![0f32; FRAME_SAMPLES];
                    while !stopped.load(Ordering::Relaxed) {
                        for pair in frame.chunks_mut(CHANNELS) {
                            let v = (phase * TAU).sin() * 0.3;
                            pair.fill(v);
                            phase = (phase + 440.0 / RATE as f32).fract();
                        }
                        on_frame(&frame);
                        std::thread::sleep(Duration::from_millis(20));
                    }
                })
                .map_err(|e| AudioError::Device(e.to_string()))?;
            Ok(Capture { stop, _platform: None })
        }
        Source::System => {
            let mut framer = Framer::default();
            let platform =
                platform_capture(stop.clone(), Box::new(move |samples: &[f32]| framer.push(samples, &mut on_frame)))?;
            Ok(Capture { stop, _platform: Some(platform) })
        }
    }
}

type SampleSink = Box<dyn FnMut(&[f32]) + Send>;

#[cfg(target_os = "linux")]
fn platform_capture(stop: Arc<AtomicBool>, sink: SampleSink) -> Result<Box<dyn Send>, AudioError> {
    linux::capture(stop, sink)
}

#[cfg(windows)]
fn platform_capture(stop: Arc<AtomicBool>, sink: SampleSink) -> Result<Box<dyn Send>, AudioError> {
    windows::capture(stop, sink)
}

#[cfg(not(any(target_os = "linux", windows)))]
fn platform_capture(_: Arc<AtomicBool>, _: SampleSink) -> Result<Box<dyn Send>, AudioError> {
    Err(AudioError::Device("not supported".into()))
}

/// Plays what arrives in `ring` on the default output until dropped.
pub struct Playback {
    stop: Arc<AtomicBool>,
    _platform: Box<dyn Send>,
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

pub fn play(ring: Arc<SampleRing>) -> Result<Playback, AudioError> {
    let stop = Arc::new(AtomicBool::new(false));
    #[cfg(target_os = "linux")]
    let platform = linux::play(stop.clone(), ring)?;
    #[cfg(windows)]
    let platform = windows::play(stop.clone(), ring)?;
    #[cfg(not(any(target_os = "linux", windows)))]
    let platform: Box<dyn Send> = {
        let _ = ring;
        return Err(AudioError::Device("not supported".into()));
    };
    Ok(Playback { stop, _platform: platform })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framer_emits_whole_frames() {
        let mut framer = Framer::default();
        let mut frames = 0;
        framer.push(&vec![0.0; FRAME_SAMPLES - 10], |_| frames += 1);
        assert_eq!(frames, 0);
        framer.push(&vec![0.0; FRAME_SAMPLES + 20], |f| {
            assert_eq!(f.len(), FRAME_SAMPLES);
            frames += 1
        });
        assert_eq!(frames, 2);
    }

    #[test]
    fn opus_round_trip_keeps_a_tone() {
        let mut enc = Encoder::new(96_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut phase = 0f32;
        let mut energy = 0f32;
        for i in 0..20 {
            let frame: Vec<f32> = (0..FRAME)
                .flat_map(|_| {
                    let v = (phase * TAU).sin() * 0.5;
                    phase = (phase + 440.0 / RATE as f32).fract();
                    [v, v]
                })
                .collect();
            let packet = enc.encode(&frame).unwrap();
            assert!(packet.len() < 1000);
            let out = dec.decode(&packet).unwrap();
            assert_eq!(out.len(), FRAME_SAMPLES);
            if i > 5 {
                energy += out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32;
            }
        }
        // A 0.5 sine has mean power 0.125.
        let mean = energy / 14.0;
        assert!((0.08..0.17).contains(&mean), "{mean}");
        assert_eq!(dec.conceal().unwrap().len(), FRAME_SAMPLES);
    }
}
