//! PipeWire capture of the default output's monitor and playback.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;

use crate::{AudioError, CHANNELS, RATE, SampleRing, SampleSink};

struct Stop(pw::channel::Sender<()>);

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

fn format_pod() -> Vec<u8> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(RATE);
    info.set_channels(CHANNELS as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = pw::spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = pw::spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let obj = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .expect("pod serialises")
        .0
        .into_inner()
}

fn run_stream(
    name: &'static str,
    capture: bool,
    stop: pw::channel::Receiver<()>,
    mut process: impl FnMut(&mut pw::buffer::Buffer<'_>) + 'static,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => if capture { "Capture" } else { "Playback" },
        *pw::keys::MEDIA_ROLE => "Communication",
        *pw::keys::APP_NAME => "Okno",
        *pw::keys::NODE_LATENCY => "960/48000",
    };
    if capture {
        // Record what the default output plays, not a microphone.
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    let stream = pw::stream::StreamBox::new(&core, name, props)?;
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            if let Some(mut buffer) = stream.dequeue_buffer() {
                process(&mut buffer);
            }
        })
        .register()?;
    let quit = mainloop.clone();
    let _stop = stop.attach(mainloop.loop_(), move |_| quit.quit());
    let bytes = format_pod();
    let mut params = [Pod::from_bytes(&bytes).expect("valid pod")];
    stream.connect(
        if capture { spa::utils::Direction::Input } else { spa::utils::Direction::Output },
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;
    mainloop.run();
    Ok(())
}

pub fn capture(_stop: Arc<AtomicBool>, mut sink: SampleSink) -> Result<Box<dyn Send>, AudioError> {
    let (tx, rx) = pw::channel::channel::<()>();
    std::thread::Builder::new()
        .name("okno-audio-capture".into())
        .spawn(move || {
            let mut samples = Vec::new();
            let result = run_stream("okno-capture", true, rx, move |buffer| {
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data() else { return };
                let end = (offset + size).min(bytes.len());
                samples.clear();
                samples.extend(
                    bytes[offset.min(end)..end].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                );
                sink(&samples);
            });
            if let Err(e) = result {
                tracing::warn!("audio capture failed: {e}");
            }
        })
        .map_err(|e| AudioError::Device(e.to_string()))?;
    Ok(Box::new(Stop(tx)))
}

pub fn play(_stop: Arc<AtomicBool>, ring: Arc<SampleRing>) -> Result<Box<dyn Send>, AudioError> {
    let (tx, rx) = pw::channel::channel::<()>();
    std::thread::Builder::new()
        .name("okno-audio-playback".into())
        .spawn(move || {
            let mut samples = Vec::new();
            let result = run_stream("okno-playback", false, rx, move |buffer| {
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else { return };
                let stride = 4 * CHANNELS;
                let Some(bytes) = data.data() else { return };
                // One quantum (NODE_LATENCY) per cycle; filling the whole,
                // possibly larger buffer would add latency.
                let frames = (bytes.len() / stride).min(crate::FRAME);
                samples.resize(frames * CHANNELS, 0.0);
                ring.pull(&mut samples);
                for (dst, s) in bytes.chunks_exact_mut(4).zip(&samples) {
                    dst.copy_from_slice(&s.to_le_bytes());
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.stride_mut() = stride as i32;
                *chunk.size_mut() = (frames * stride) as u32;
            });
            if let Err(e) = result {
                tracing::warn!("audio playback failed: {e}");
            }
        })
        .map_err(|e| AudioError::Device(e.to_string()))?;
    Ok(Box::new(Stop(tx)))
}
