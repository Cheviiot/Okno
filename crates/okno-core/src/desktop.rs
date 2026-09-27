//! Host-side remote desktop service: streams a display and injects input.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use okno_codec::{EncoderSettings, VideoEncoder};
use okno_desktop::{Desktop, Taken};
use okno_net::{Sender, TrySendError};
use okno_proto::envelope::Msg;
use okno_proto::{AudioPacket, ClipboardText, Codec, Display, VideoFrame, VideoStart};

use crate::files::FileService;
use crate::host::{BoxFuture, HostSession, SessionHandler};
use crate::terminal::TerminalService;
use crate::tunnel::TunnelService;

pub const SERVICE_DESKTOP: &str = "desktop";

/// Serves the remote desktop on top of the control messages.
pub struct DesktopHandler {
    desktop: Arc<dyn Desktop>,
    incoming: PathBuf,
    audio: okno_audio::Source,
}

impl DesktopHandler {
    pub fn new(desktop: Arc<dyn Desktop>) -> Self {
        Self { desktop, incoming: FileService::default_incoming(), audio: okno_audio::Source::System }
    }

    /// Where session sound comes from (a tone in tests).
    pub fn with_audio(mut self, source: okno_audio::Source) -> Self {
        self.audio = source;
        self
    }

    /// Directory for files sent by clients.
    pub fn with_incoming(mut self, dir: PathBuf) -> Self {
        self.incoming = dir;
        self
    }
}

impl SessionHandler for DesktopHandler {
    fn displays(&self) -> Vec<Display> {
        self.desktop
            .displays()
            .into_iter()
            .map(|d| Display { id: d.id, name: d.name, width: d.width, height: d.height, primary: d.primary })
            .collect()
    }

    fn run(&self, session: HostSession) -> BoxFuture {
        Box::pin(serve(self.desktop.clone(), self.incoming.clone(), self.audio, session))
    }
}

/// Aborts a task when dropped.
struct TaskGuard(tokio::task::JoinHandle<()>);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn serve(
    desktop: Arc<dyn Desktop>,
    incoming: PathBuf,
    audio_source: okno_audio::Source,
    mut session: HostSession,
) -> Result<(), String> {
    let mut stream: Option<Streamer> = None;
    // Kept alive while enabled; dropping it stops the capture.
    let mut _audio: Option<okno_audio::Capture> = None;
    let mut files = FileService::new(session.sender.clone(), incoming);
    let mut terminals = TerminalService::new(session.sender.clone());
    let mut tunnels = TunnelService::new(session.sender.clone());

    // Clipboard: host copies go to the client, client texts to the host.
    let clipboard = desktop.clipboard();
    let _forward_copies = clipboard.as_ref().map(|link| {
        let sender = session.sender.clone();
        let mut copied = link.copied.clone();
        TaskGuard(tokio::spawn(async move {
            while copied.changed().await.is_ok() {
                let text = copied.borrow_and_update().clone();
                if let Some(text) = text {
                    if sender.send(Msg::Clipboard(ClipboardText { text: text.to_string() })).await.is_err() {
                        break;
                    }
                }
            }
        }))
    });
    loop {
        let msg = tokio::select! {
            msg = session.receiver.recv() => msg,
            _ = session.shutdown.changed() => return Ok(()),
        };
        match msg {
            Ok(Msg::Input(event)) => desktop.inject(event),
            Ok(Msg::File(request)) => files.handle(request).await,
            Ok(Msg::AudioControl(control)) => {
                _audio = None;
                if control.enabled {
                    match start_audio(audio_source, session.sender.clone()) {
                        Ok(capture) => _audio = Some(capture),
                        Err(e) => tracing::warn!("sound capture unavailable: {e}"),
                    }
                }
            }
            Ok(Msg::Terminal(request)) => terminals.handle(request).await,
            Ok(Msg::Tunnel(request)) => tunnels.handle(request).await,
            Ok(Msg::Clipboard(c)) => {
                if let Some(link) = &clipboard {
                    if c.text.len() <= okno_desktop::MAX_CLIPBOARD {
                        let _ = link.paste.send(c.text);
                    }
                }
            }
            Ok(Msg::VideoStart(start)) => {
                stream = None; // stop the previous one first
                match Streamer::start(desktop.clone(), session.sender.clone(), start) {
                    Ok(s) => stream = Some(s),
                    Err(e) => {
                        let _ = session.sender.send(Msg::Error(okno_proto::ErrorMsg { message: e })).await;
                    }
                }
            }
            Ok(Msg::VideoStop(_)) => stream = None,
            Ok(Msg::KeyframeRequest(_)) => {
                if let Some(s) = &stream {
                    s.shared.keyframe.store(true, Ordering::Relaxed);
                }
            }
            Ok(Msg::Stats(stats)) => tracing::trace!("client stats: {stats:?}"),
            Ok(Msg::Ping(p)) => session.sender.send(Msg::Pong(p)).await.map_err(|e| e.to_string())?,
            Ok(Msg::Close(_)) | Err(okno_net::Error::Closed) => return Ok(()),
            Ok(other) => tracing::debug!("unhandled message {other:?}"),
            Err(e) => return Err(e.to_string()),
        }
    }
}

/// Captures host sound and sends it as Opus packets. Packets that do not
/// fit in the audio queue are dropped: late sound is useless.
fn start_audio(source: okno_audio::Source, sender: Sender) -> Result<okno_audio::Capture, okno_audio::AudioError> {
    let mut encoder = okno_audio::Encoder::new(96_000)?;
    let mut seq = 0u64;
    okno_audio::capture(source, move |frame| match encoder.encode(frame) {
        Ok(data) => {
            let _ = sender.try_send(Msg::Audio(AudioPacket { seq, data }));
            seq += 1;
        }
        Err(e) => tracing::debug!("audio encode: {e}"),
    })
}

/// Adapts the encoder bitrate to the link: back off quickly when frames
/// do not fit in the send queue, recover slowly while everything fits.
struct RateControl {
    max: u32,
    min: u32,
    current: u32,
    drops: u32,
    last_change: Instant,
    calm_since: Instant,
}

impl RateControl {
    const STEP: Duration = Duration::from_secs(3);
    const CALM: Duration = Duration::from_secs(6);

    fn new(max: u32, now: Instant) -> Self {
        Self { max, min: (max / 8).max(500), current: max, drops: 0, last_change: now, calm_since: now }
    }

    fn dropped(&mut self, now: Instant) {
        self.drops += 1;
        self.calm_since = now;
    }

    /// The new bitrate when it should change.
    fn tick(&mut self, now: Instant) -> Option<u32> {
        if now.duration_since(self.last_change) < Self::STEP {
            return None;
        }
        let before = self.current;
        if self.drops >= 2 {
            self.current = (self.current.saturating_mul(6) / 10).max(self.min);
        } else if self.drops == 0 && now.duration_since(self.calm_since) >= Self::CALM {
            self.current = (self.current.saturating_mul(5) / 4).min(self.max);
        }
        self.drops = 0;
        if self.current == before {
            return None;
        }
        self.last_change = now;
        Some(self.current)
    }
}

struct Shared {
    stop: AtomicBool,
    keyframe: AtomicBool,
    dropped: AtomicU32,
}

/// Capture → encode → send on a blocking thread; stops when dropped.
struct Streamer {
    shared: Arc<Shared>,
}

impl Drop for Streamer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl Streamer {
    fn start(desktop: Arc<dyn Desktop>, sender: Sender, start: VideoStart) -> Result<Self, String> {
        let capture = desktop.capture(start.display).map_err(|e| e.to_string())?;
        let settings = EncoderSettings {
            max_fps: if start.max_fps == 0 { 30 } else { start.max_fps.min(60) },
            bitrate_kbps: if start.bitrate_kbps == 0 { 8000 } else { start.bitrate_kbps.clamp(500, 100_000) },
        };
        let mut encoder = VideoEncoder::new(settings).map_err(|e| e.to_string())?;
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            keyframe: AtomicBool::new(false),
            dropped: AtomicU32::new(0),
        });
        let state = shared.clone();
        let display_id = start.display;
        tokio::task::spawn_blocking(move || {
            // Own the whole capture: a closure would otherwise capture only
            // `capture.slot` and drop the guard that keeps capturing alive.
            let capture = capture;
            let interval = Duration::from_secs(1) / settings.max_fps;
            let epoch = Instant::now();
            let mut rate = RateControl::new(settings.bitrate_kbps, epoch);
            let mut next = Instant::now();
            while !state.stop.load(Ordering::Relaxed) {
                let frame = match capture.slot.take(Duration::from_millis(250)) {
                    Taken::Frame(f) => f,
                    Taken::Timeout => continue,
                    Taken::Closed => break,
                };
                if let Some(kbps) = rate.tick(Instant::now()) {
                    tracing::debug!("video bitrate now {kbps} kbit/s");
                    // A new encoder starts with a keyframe.
                    if let Err(e) = encoder.reconfigure(EncoderSettings { bitrate_kbps: kbps, ..settings }) {
                        tracing::warn!("encoder reconfigure failed: {e}");
                    }
                }
                if state.keyframe.swap(false, Ordering::Relaxed) {
                    encoder.request_keyframe();
                }
                let pts = epoch.elapsed().as_micros() as u64;
                let packet = match encoder.encode(&frame, pts) {
                    Ok(Some(p)) => p,
                    Ok(None) => continue,
                    Err(e) => {
                        tracing::warn!("encode failed: {e}");
                        break;
                    }
                };
                let msg = Msg::Video(VideoFrame {
                    display: display_id,
                    codec: Codec::H264 as i32,
                    width: packet.width,
                    height: packet.height,
                    keyframe: packet.keyframe,
                    pts_us: pts,
                    data: packet.data,
                });
                match sender.try_send(msg) {
                    Ok(()) => {}
                    // The link is slower than the encoder: a lost P-frame
                    // breaks the reference chain, so restart from a keyframe.
                    Err(TrySendError::Full) => {
                        state.dropped.fetch_add(1, Ordering::Relaxed);
                        rate.dropped(Instant::now());
                        encoder.request_keyframe();
                    }
                    Err(TrySendError::Closed) => break,
                }
                next += interval;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                } else {
                    next = now;
                }
            }
            tracing::debug!("video stream of display {display_id} stopped");
        });
        Ok(Self { shared })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_backs_off_and_recovers() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(8000, t0);
        assert_eq!(rc.tick(t0 + Duration::from_secs(1)), None);
        rc.dropped(t0 + Duration::from_secs(2));
        rc.dropped(t0 + Duration::from_secs(2));
        assert_eq!(rc.tick(t0 + Duration::from_secs(3)), Some(4800));
        rc.dropped(t0 + Duration::from_secs(4));
        rc.dropped(t0 + Duration::from_secs(4));
        // Too soon after the last change.
        assert_eq!(rc.tick(t0 + Duration::from_secs(5)), None);
        assert_eq!(rc.tick(t0 + Duration::from_secs(6)), Some(2880));
        // Calm for long enough: step back up, never above the maximum.
        assert_eq!(rc.tick(t0 + Duration::from_secs(9)), None);
        assert_eq!(rc.tick(t0 + Duration::from_secs(12)), Some(3600));
        let mut t = t0 + Duration::from_secs(12);
        for _ in 0..20 {
            t += Duration::from_secs(3);
            rc.tick(t);
        }
        assert_eq!(rc.current, 8000);
    }

    #[test]
    fn rate_has_a_floor() {
        let t0 = Instant::now();
        let mut rc = RateControl::new(8000, t0);
        let mut t = t0;
        for _ in 0..30 {
            t += Duration::from_secs(3);
            rc.dropped(t);
            rc.dropped(t);
            rc.tick(t);
        }
        assert_eq!(rc.current, 1000);
    }
}
