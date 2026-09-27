//! WASAPI through cpal: loopback capture of the default output, playback.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use crate::{AudioError, CHANNELS, RATE, SampleRing, SampleSink};

fn err(e: impl std::fmt::Display) -> AudioError {
    AudioError::Device(e.to_string())
}

/// cpal streams are not `Send` on Windows, so each lives on its own thread
/// until the stop flag is raised.
fn keep_on_thread(
    name: &str,
    stop: Arc<AtomicBool>,
    build: impl FnOnce() -> Result<cpal::Stream, AudioError> + Send + 'static,
) -> Result<Box<dyn Send>, AudioError> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let stream = match build().and_then(|s| s.play().map(|_| s).map_err(err)) {
                Ok(s) => {
                    let _ = ready_tx.send(Ok(()));
                    s
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(100));
            }
            drop(stream);
        })
        .map_err(err)?;
    ready_rx.recv().map_err(err)??;
    Ok(Box::new(()))
}

/// Converts any channel count to interleaved stereo.
fn to_stereo(input: &[f32], channels: usize, out: &mut Vec<f32>) {
    out.clear();
    for frame in input.chunks(channels.max(1)) {
        let l = frame[0];
        let r = if channels > 1 { frame[1] } else { l };
        out.extend_from_slice(&[l, r]);
    }
}

pub fn capture(stop: Arc<AtomicBool>, mut sink: SampleSink) -> Result<Box<dyn Send>, AudioError> {
    keep_on_thread("okno-audio-capture", stop, move || {
        let device = cpal::default_host().default_output_device().ok_or_else(|| err("no output device"))?;
        let config = device.default_output_config().map_err(err)?;
        let channels = config.channels() as usize;
        if config.sample_rate().0 != RATE {
            tracing::warn!("output runs at {} Hz; sound will be pitched", config.sample_rate().0);
        }
        let mut stereo = Vec::new();
        // WASAPI loopback: an input stream on an output device.
        device
            .build_input_stream(
                &config.config(),
                move |data: &[f32], _| {
                    to_stereo(data, channels, &mut stereo);
                    sink(&stereo);
                },
                |e| tracing::warn!("capture: {e}"),
                None,
            )
            .map_err(err)
    })
}

pub fn play(stop: Arc<AtomicBool>, ring: Arc<SampleRing>) -> Result<Box<dyn Send>, AudioError> {
    keep_on_thread("okno-audio-playback", stop, move || {
        let device = cpal::default_host().default_output_device().ok_or_else(|| err("no output device"))?;
        let config = cpal::StreamConfig {
            channels: CHANNELS as u16,
            sample_rate: cpal::SampleRate(RATE),
            buffer_size: cpal::BufferSize::Default,
        };
        device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| ring.pull(data),
                |e| tracing::warn!("playback: {e}"),
                None,
            )
            .map_err(err)
    })
}
