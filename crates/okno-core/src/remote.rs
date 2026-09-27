//! Client-side runtime of a logged-in session: receives and decodes video,
//! sends input, and reports everything else as [`RemoteEvent`]s.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;

use okno_codec::VideoDecoder;
use okno_net::{Receiver, Sender};
use okno_proto::envelope::Msg;
use okno_proto::{
    AudioControl, AudioPacket, Close, InputEvent, KeyframeRequest, Ping, VideoFrame, VideoStart, VideoStop,
};
use tokio::task::JoinHandle;

use crate::client::Session;
use crate::files::Files;
use crate::terminal::Terminals;
use crate::tunnel::Tunnels;

/// Decoded packets waiting for the decoder. When full, the reader drops
/// frames until the next keyframe instead of building latency.
const DECODE_QUEUE: usize = 8;

#[derive(Debug)]
pub enum RemoteEvent {
    /// A decoded frame, packed RGBA.
    Frame {
        display: u32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    Clipboard(String),
    Error(String),
    /// The session ended; `None` when closed normally.
    Closed(Option<String>),
    /// Any other message, for services layered on top.
    Message(Msg),
}

pub type EventSink = Arc<dyn Fn(RemoteEvent) + Send + Sync>;

/// Cumulative counters of a session; diff two snapshots for rates.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub frames: u64,
    pub video_bytes: u64,
    /// Round trip of the last ping.
    pub rtt: Option<std::time::Duration>,
}

#[derive(Default)]
struct Counters {
    frames: AtomicU64,
    video_bytes: AtomicU64,
    rtt_us: AtomicU64,
    ping: std::sync::Mutex<Option<(u64, std::time::Instant)>>,
}

const PING_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// Handle to a running session.
pub struct Remote {
    sender: Sender,
    counters: Arc<Counters>,
    pinger: JoinHandle<()>,
    audio: AudioIn,
    files: Files,
    terminals: Terminals,
    tunnels: Tunnels,
    reader: JoinHandle<()>,
}

impl Session {
    /// Starts the background reader. `events` is called from worker threads.
    pub fn run(self, events: EventSink) -> Remote {
        let services = Services {
            files: Files::new(self.sender.clone()),
            terminals: Terminals::new(self.sender.clone()),
            tunnels: Tunnels::new(self.sender.clone()),
        };
        let Services { files, terminals, tunnels } = services.clone();
        let audio = AudioIn::default();
        let counters = Arc::new(Counters::default());
        let reader = tokio::spawn(read_loop(
            self.receiver,
            self.sender.clone(),
            services,
            audio.clone(),
            counters.clone(),
            events,
        ));
        let pinger = {
            let sender = self.sender.clone();
            let counters = counters.clone();
            tokio::spawn(async move {
                let mut nonce = 0u64;
                loop {
                    tokio::time::sleep(PING_EVERY).await;
                    nonce += 1;
                    *counters.ping.lock().unwrap() = Some((nonce, std::time::Instant::now()));
                    if sender.try_send(Msg::Ping(Ping { nonce })).is_err() && sender.is_closed() {
                        break;
                    }
                }
            })
        };
        Remote { sender: self.sender, counters, pinger, audio, files, terminals, tunnels, reader }
    }
}

impl Remote {
    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    /// Starts host sound; decoded samples go to `ring` (play it with
    /// [`okno_audio::play`]).
    pub fn start_audio(&self, ring: Arc<okno_audio::SampleRing>) -> Result<(), okno_audio::AudioError> {
        *self.audio.0.lock().unwrap() = Some(AudioState { decoder: okno_audio::Decoder::new()?, ring, next: None });
        let _ = self.sender.try_send(Msg::AudioControl(AudioControl {
            enabled: true,
            sample_rate: okno_audio::RATE,
            channels: 2,
        }));
        Ok(())
    }

    pub fn stop_audio(&self) {
        self.audio.0.lock().unwrap().take();
        let _ = self.sender.try_send(Msg::AudioControl(AudioControl { enabled: false, ..Default::default() }));
    }

    /// File transfer with the host.
    pub fn files(&self) -> Files {
        self.files.clone()
    }

    /// Remote shells.
    pub fn terminals(&self) -> Terminals {
        self.terminals.clone()
    }

    /// TCP port forwarding through the host.
    pub fn tunnels(&self) -> Tunnels {
        self.tunnels.clone()
    }

    pub async fn start_video(&self, display: u32, max_fps: u32, bitrate_kbps: u32) -> Result<(), okno_net::Error> {
        self.sender.send(Msg::VideoStart(VideoStart { display, max_fps, bitrate_kbps })).await
    }

    /// Like [`start_video`](Self::start_video) without waiting; for UI code.
    pub fn request_video(&self, display: u32, max_fps: u32, bitrate_kbps: u32) {
        let _ = self.sender.try_send(Msg::VideoStart(VideoStart { display, max_fps, bitrate_kbps }));
    }

    pub async fn stop_video(&self) -> Result<(), okno_net::Error> {
        self.sender.send(Msg::VideoStop(VideoStop {})).await
    }

    pub fn stats(&self) -> Stats {
        let rtt = self.counters.rtt_us.load(Ordering::Relaxed);
        Stats {
            frames: self.counters.frames.load(Ordering::Relaxed),
            video_bytes: self.counters.video_bytes.load(Ordering::Relaxed),
            rtt: (rtt > 0).then(|| std::time::Duration::from_micros(rtt)),
        }
    }

    /// Queues an input event without waiting. Motion is dropped when the
    /// input queue is full; later motion supersedes it anyway.
    pub fn send_input(&self, event: InputEvent) {
        let _ = self.sender.try_send(Msg::Input(event));
    }

    /// Puts text on the remote clipboard.
    pub fn send_clipboard(&self, text: String) {
        let _ = self.sender.try_send(Msg::Clipboard(okno_proto::ClipboardText { text }));
    }

    pub async fn close(self) {
        self.pinger.abort();
        let _ = self.sender.send(Msg::Close(Close { reason: "closed by user".into() })).await;
        drop(self.sender);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.reader).await;
    }
}

struct AudioState {
    decoder: okno_audio::Decoder,
    ring: Arc<okno_audio::SampleRing>,
    next: Option<u64>,
}

/// Decodes incoming sound while enabled.
#[derive(Clone, Default)]
struct AudioIn(Arc<std::sync::Mutex<Option<AudioState>>>);

impl AudioIn {
    fn packet(&self, packet: AudioPacket) {
        let mut guard = self.0.lock().unwrap();
        let Some(state) = guard.as_mut() else { return };
        // Conceal a few lost packets; after a long gap just resume.
        if let Some(expected) = state.next {
            let lost = packet.seq.saturating_sub(expected);
            if (1..=5).contains(&lost) {
                for _ in 0..lost {
                    if let Ok(samples) = state.decoder.conceal() {
                        state.ring.push(samples);
                    }
                }
            }
        }
        state.next = Some(packet.seq + 1);
        match state.decoder.decode(&packet.data) {
            Ok(samples) => state.ring.push(samples),
            Err(e) => tracing::debug!("audio decode: {e}"),
        }
    }
}

/// Per-request routers of the services layered on the session.
#[derive(Clone)]
struct Services {
    files: Files,
    terminals: Terminals,
    tunnels: Tunnels,
}

async fn read_loop(
    mut receiver: Receiver,
    sender: Sender,
    services: Services,
    audio: AudioIn,
    counters: Arc<Counters>,
    events: EventSink,
) {
    let (packets, queue) = std_mpsc::sync_channel::<VideoFrame>(DECODE_QUEUE);
    let decoder_events = events.clone();
    let decoder_sender = sender.clone();
    let decoder_counters = counters.clone();
    std::thread::Builder::new()
        .name("okno-decoder".into())
        .spawn(move || decode_loop(queue, decoder_sender, decoder_counters, decoder_events))
        .expect("spawn decoder thread");

    let mut skip_until_keyframe = false;
    let reason = loop {
        match receiver.recv().await {
            Ok(Msg::Video(frame)) => {
                counters.video_bytes.fetch_add(frame.data.len() as u64, Ordering::Relaxed);
                if skip_until_keyframe && !frame.keyframe {
                    continue;
                }
                skip_until_keyframe = false;
                if let Err(std_mpsc::TrySendError::Full(_)) = packets.try_send(frame) {
                    skip_until_keyframe = true;
                    let _ = sender.try_send(Msg::KeyframeRequest(KeyframeRequest {}));
                }
            }
            Ok(Msg::Clipboard(c)) => events(RemoteEvent::Clipboard(c.text)),
            Ok(Msg::Audio(packet)) => audio.packet(packet),
            Ok(Msg::File(reply)) => services.files.dispatch(reply),
            Ok(Msg::Terminal(reply)) => services.terminals.dispatch(reply),
            Ok(Msg::Tunnel(reply)) => services.tunnels.dispatch(reply),
            Ok(Msg::Error(e)) => events(RemoteEvent::Error(e.message)),
            Ok(Msg::Ping(p)) => {
                let _ = sender.send(Msg::Pong(p)).await;
            }
            Ok(Msg::Pong(p)) => {
                let sent = *counters.ping.lock().unwrap();
                if let Some((_, at)) = sent.filter(|(n, _)| *n == p.nonce) {
                    counters.rtt_us.store(at.elapsed().as_micros().max(1) as u64, Ordering::Relaxed);
                }
            }
            Ok(Msg::Close(c)) => break Some(c.reason).filter(|r| !r.is_empty()),
            Ok(other) => events(RemoteEvent::Message(other)),
            Err(okno_net::Error::Closed) => break None,
            Err(e) => break Some(e.to_string()),
        }
    };
    drop(packets);
    events(RemoteEvent::Closed(reason));
}

fn decode_loop(queue: std_mpsc::Receiver<VideoFrame>, sender: Sender, counters: Arc<Counters>, events: EventSink) {
    let mut decoder = match VideoDecoder::new() {
        Ok(d) => d,
        Err(e) => {
            events(RemoteEvent::Error(format!("video decoder: {e}")));
            return;
        }
    };
    let mut broken = false;
    while let Ok(frame) = queue.recv() {
        if broken && !frame.keyframe {
            continue;
        }
        match decoder.decode(&frame.data) {
            Ok(Some(image)) => {
                broken = false;
                counters.frames.fetch_add(1, Ordering::Relaxed);
                events(RemoteEvent::Frame {
                    display: frame.display,
                    width: image.width,
                    height: image.height,
                    rgba: image.rgba.to_vec(),
                });
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!("decode error: {e}; asking for a keyframe");
                broken = true;
                let _ = sender.try_send(Msg::KeyframeRequest(KeyframeRequest {}));
            }
        }
    }
}
