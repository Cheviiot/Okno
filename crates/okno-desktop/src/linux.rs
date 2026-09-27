//! Linux desktop through XDG portals.
//!
//! One RemoteDesktop portal session carries keyboard and pointer access and
//! the attached ScreenCast streams (one per monitor). Frames come from
//! PipeWire in shared memory; each capture runs its own PipeWire main loop on
//! a dedicated thread. With persist mode "until revoked" the portal returns a
//! restore token, so after the first approval later sessions start without a
//! dialog.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use ashpd::desktop::clipboard::{Clipboard, SetSelectionOptions};
use ashpd::desktop::remote_desktop::{Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use futures_util::{Stream, StreamExt};
use okno_proto::input_event::Event;
use okno_proto::{InputEvent, MouseButton};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::Pod;
use tokio::sync::{mpsc, watch};

use crate::keymap::button;
use crate::{
    Capture, ClipboardLink, Desktop, DesktopError, DisplayInfo, FrameSlot, MAX_CLIPBOARD, OpenOptions, OpenedDesktop,
    PixelFormat, RawFrame,
};

#[derive(Clone, Debug)]
struct Monitor {
    info: DisplayInfo,
    node: u32,
    /// Logical size used for absolute pointer coordinates.
    logical: (f64, f64),
}

pub struct PortalDesktop {
    monitors: Vec<Monitor>,
    pipewire: OwnedFd,
    input: mpsc::UnboundedSender<InputEvent>,
    clipboard: Option<ClipboardLink>,
}

fn portal_error(e: ashpd::Error) -> DesktopError {
    match e {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => DesktopError::Denied,
        other => DesktopError::Unsupported(other.to_string()),
    }
}

impl PortalDesktop {
    pub async fn open(options: OpenOptions) -> Result<OpenedDesktop, DesktopError> {
        let remote = RemoteDesktop::new().await.map_err(portal_error)?;
        let screencast = Screencast::new().await.map_err(portal_error)?;
        let session = remote.create_session(Default::default()).await.map_err(portal_error)?;
        remote
            .select_devices(
                &session,
                SelectDevicesOptions::default()
                    .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(options.restore_token.as_deref()),
            )
            .await
            .map_err(portal_error)?
            .response()
            .map_err(portal_error)?;
        screencast
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Embedded)
                    .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
                    .set_multiple(true),
            )
            .await
            .map_err(portal_error)?
            .response()
            .map_err(portal_error)?;
        // Clipboard sharing must be requested before the session starts.
        let clipboard = match Clipboard::new().await {
            Ok(c) => match c.request(&session, Default::default()).await {
                Ok(()) => Some(c),
                Err(e) => {
                    tracing::debug!("clipboard portal refused: {e}");
                    None
                }
            },
            Err(e) => {
                tracing::debug!("no clipboard portal: {e}");
                None
            }
        };
        let selected = remote
            .start(&session, None, Default::default())
            .await
            .map_err(portal_error)?
            .response()
            .map_err(portal_error)?;
        if !selected.devices().contains(DeviceType::Pointer) {
            tracing::warn!("pointer control was not granted");
        }

        let monitors: Vec<Monitor> = selected
            .streams()
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (w, h) = s.size().unwrap_or((0, 0));
                Monitor {
                    info: DisplayInfo {
                        id: i as u32,
                        name: s.id().map(str::to_owned).unwrap_or_else(|| format!("Monitor {}", i + 1)),
                        width: w.max(0) as u32,
                        height: h.max(0) as u32,
                        primary: s.position() == Some((0, 0)),
                    },
                    node: s.pipe_wire_node_id(),
                    logical: (w as f64, h as f64),
                }
            })
            .collect();
        if monitors.is_empty() {
            return Err(DesktopError::Denied);
        }
        let pipewire = screencast.open_pipe_wire_remote(&session, Default::default()).await.map_err(portal_error)?;

        let (input, events) = mpsc::unbounded_channel();
        let clipboard = clipboard.filter(|_| selected.is_clipboard_enabled());
        let (copied_tx, copied) = watch::channel(None);
        let (paste, paste_rx) = mpsc::unbounded_channel();
        let link = clipboard.is_some().then(|| ClipboardLink { copied, paste });
        tokio::spawn(session_loop(remote, session, monitors.clone(), events, clipboard, copied_tx, paste_rx));

        let restore_token = selected.restore_token().map(str::to_owned);
        let desktop = Arc::new(PortalDesktop { monitors, pipewire, input, clipboard: link });
        Ok(OpenedDesktop { desktop, restore_token })
    }
}

impl Desktop for PortalDesktop {
    fn displays(&self) -> Vec<DisplayInfo> {
        self.monitors.iter().map(|m| m.info.clone()).collect()
    }

    fn capture(&self, display: u32) -> Result<Capture, DesktopError> {
        let monitor = self.monitors.get(display as usize).ok_or(DesktopError::NoDisplay(display))?;
        let fd = self.pipewire.try_clone().map_err(|e| DesktopError::Capture(e.to_string()))?;
        let slot = Arc::new(FrameSlot::default());
        let (stop_tx, stop_rx) = pw::channel::channel::<()>();
        let node = monitor.node;
        let producer = slot.clone();
        std::thread::Builder::new()
            .name("okno-pipewire".into())
            .spawn(move || {
                if let Err(e) = capture_thread(fd, node, producer.clone(), stop_rx) {
                    tracing::warn!("PipeWire capture failed: {e}");
                }
                producer.close();
            })
            .map_err(|e| DesktopError::Capture(e.to_string()))?;
        Ok(Capture::new(slot, StopCapture(stop_tx)))
    }

    fn inject(&self, event: InputEvent) {
        let _ = self.input.send(event);
    }

    fn clipboard(&self) -> Option<ClipboardLink> {
        self.clipboard.clone()
    }
}

struct StopCapture(pw::channel::Sender<()>);

impl Drop for StopCapture {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

const TEXT_MIME: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING"];

/// Serves the portal session: injects input in order and bridges the
/// clipboard. Ends (and closes the portal session) when the desktop is
/// dropped.
async fn session_loop(
    remote: RemoteDesktop,
    session: Session<RemoteDesktop>,
    monitors: Vec<Monitor>,
    mut events: mpsc::UnboundedReceiver<InputEvent>,
    clipboard: Option<Clipboard>,
    copied: watch::Sender<Option<Arc<str>>>,
    mut paste: mpsc::UnboundedReceiver<String>,
) {
    // Only one portal session exists per process, so signals need no
    // filtering by session path (which ashpd does not expose).
    let mut owner_changes = match &clipboard {
        Some(c) => c.receive_selection_owner_changed::<RemoteDesktop>().await.ok().map(Box::pin),
        None => None,
    };
    let mut transfers = match &clipboard {
        Some(c) => c.receive_selection_transfer::<RemoteDesktop>().await.ok().map(Box::pin),
        None => None,
    };
    // Text we offered to the host clipboard, served on request.
    let mut offered: Option<String> = None;

    loop {
        tokio::select! {
            event = events.recv() => match event {
                Some(event) => {
                    if let Err(e) = inject(&remote, &session, &monitors, event).await {
                        tracing::debug!("portal input failed: {e}");
                    }
                }
                None => break,
            },
            Some(text) = paste.recv(), if clipboard.is_some() => {
                let c = clipboard.as_ref().unwrap();
                offered = Some(text);
                if let Err(e) = c.set_selection(&session, SetSelectionOptions::default().set_mime_types(TEXT_MIME)).await {
                    tracing::debug!("clipboard set failed: {e}");
                }
            }
            Some((_, change)) = next(&mut owner_changes) => {
                if change.session_is_owner() == Some(true) {
                    continue;
                }
                let Some(mime) = TEXT_MIME.iter().find(|m| change.mime_types().iter().any(|t| t == *m)) else {
                    continue;
                };
                let c = clipboard.as_ref().unwrap();
                match c.selection_read(&session, mime).await {
                    Ok(fd) => {
                        if let Some(text) = read_text(fd.into()).await {
                            copied.send_replace(Some(Arc::from(text)));
                        }
                    }
                    Err(e) => tracing::debug!("clipboard read failed: {e}"),
                }
            }
            Some((_, _mime, serial)) = next(&mut transfers) => {
                let c = clipboard.as_ref().unwrap();
                let ok = match (&offered, c.selection_write(&session, serial).await) {
                    (Some(text), Ok(fd)) => write_text(fd.into(), text.clone()).await,
                    _ => false,
                };
                let _ = c.selection_write_done(&session, serial, ok).await;
            }
        }
    }
    let _ = session.close().await;
}

/// Next item of an optional stream; pending forever when absent.
async fn next<S: Stream + Unpin>(stream: &mut Option<S>) -> Option<S::Item> {
    match stream {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

async fn read_text(fd: OwnedFd) -> Option<String> {
    let read = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::from(fd).take(MAX_CLIPBOARD as u64).read_to_end(&mut bytes).ok()?;
        String::from_utf8(bytes).ok()
    });
    match tokio::time::timeout(std::time::Duration::from_secs(3), read).await {
        Ok(Ok(text)) => text,
        _ => None,
    }
}

async fn write_text(fd: OwnedFd, text: String) -> bool {
    let write = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        std::fs::File::from(fd).write_all(text.as_bytes()).is_ok()
    });
    matches!(tokio::time::timeout(std::time::Duration::from_secs(3), write).await, Ok(Ok(true)))
}

/// Sends one input event to the portal.
async fn inject(
    remote: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    monitors: &[Monitor],
    event: InputEvent,
) -> Result<(), ashpd::Error> {
    let Some(event) = event.event else { return Ok(()) };
    match event {
        Event::Motion(m) => {
            let Some(mon) = monitors.get(m.display as usize) else { return Ok(()) };
            if mon.logical.0 <= 0.0 {
                return Ok(());
            }
            let x = m.x.clamp(0.0, 1.0) * mon.logical.0;
            let y = m.y.clamp(0.0, 1.0) * mon.logical.1;
            remote.notify_pointer_motion_absolute(session, mon.node, x, y, Default::default()).await
        }
        Event::Button(b) => {
            let code = match MouseButton::try_from(b.button) {
                Ok(MouseButton::Left) => button::LEFT,
                Ok(MouseButton::Right) => button::RIGHT,
                Ok(MouseButton::Middle) => button::MIDDLE,
                Ok(MouseButton::Back) => button::SIDE,
                Ok(MouseButton::Forward) => button::EXTRA,
                _ => return Ok(()),
            };
            remote.notify_pointer_button(session, code, key_state(b.pressed), Default::default()).await
        }
        Event::Scroll(s) => {
            let mut result = Ok(());
            if s.steps_y != 0 {
                result =
                    remote.notify_pointer_axis_discrete(session, Axis::Vertical, s.steps_y, Default::default()).await;
            }
            if result.is_ok() && s.steps_x != 0 {
                result =
                    remote.notify_pointer_axis_discrete(session, Axis::Horizontal, s.steps_x, Default::default()).await;
            }
            if result.is_ok() && (s.dx != 0.0 || s.dy != 0.0) {
                result = remote
                    .notify_pointer_axis(
                        &session,
                        s.dx,
                        s.dy,
                        ashpd::desktop::remote_desktop::NotifyPointerAxisOptions::default().set_finish(true),
                    )
                    .await;
            }
            result
        }
        Event::Key(k) => {
            remote.notify_keyboard_keycode(session, k.evdev_code as i32, key_state(k.pressed), Default::default()).await
        }
    }
}

fn key_state(pressed: bool) -> KeyState {
    if pressed { KeyState::Pressed } else { KeyState::Released }
}

struct StreamState {
    info: spa::param::video::VideoInfoRaw,
    format: Option<(u32, u32, PixelFormat)>,
    slot: Arc<FrameSlot>,
}

fn capture_thread(
    fd: OwnedFd,
    node: u32,
    slot: Arc<FrameSlot>,
    stop: pw::channel::Receiver<()>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "okno-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let failed = Rc::new(RefCell::new(false));
    let failed_flag = failed.clone();
    let quit_on_error = mainloop.clone();
    let state = StreamState { info: Default::default(), format: None, slot };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .state_changed(move |_, state, _old, new| {
            if let pw::stream::StreamState::Error(e) = new {
                tracing::warn!("PipeWire stream error: {e}");
                *failed_flag.borrow_mut() = true;
                state.slot.close();
                quit_on_error.quit();
            }
        })
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param) else { return };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if state.info.parse(param).is_err() {
                return;
            }
            use spa::param::video::VideoFormat as F;
            let pixel = match state.info.format() {
                F::BGRx | F::BGRA => PixelFormat::Bgra,
                F::RGBx | F::RGBA => PixelFormat::Rgba,
                other => {
                    tracing::warn!("unsupported PipeWire format {other:?}");
                    state.format = None;
                    return;
                }
            };
            let size = state.info.size();
            tracing::debug!("PipeWire format {:?} {}x{}", state.info.format(), size.width, size.height);
            state.format = Some((size.width, size.height, pixel));
        })
        .process(|stream, state| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some((width, height, format)) = state.format else { return };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else { return };
            let chunk = data.chunk();
            if chunk.flags().contains(spa::buffer::ChunkFlags::CORRUPTED) || chunk.size() == 0 {
                return;
            }
            let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
            let stride = match chunk.stride() {
                s if s > 0 => s as usize,
                _ => width as usize * 4,
            };
            let Some(bytes) = data.data() else { return };
            let end = (offset + size).min(bytes.len());
            if offset >= end {
                return;
            }
            let pixels = &bytes[offset..end];
            let needed = stride * (height as usize - 1) + width as usize * 4;
            if height == 0 || pixels.len() < needed {
                return;
            }
            state.slot.put(RawFrame { width, height, stride, format, data: pixels[..needed].to_vec() });
        })
        .register()?;

    let quit = mainloop.clone();
    let _stop = stop.attach(mainloop.loop_(), move |_| quit.quit());

    let params_bytes = format_params();
    let mut params = [Pod::from_bytes(&params_bytes).expect("valid pod")];
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    mainloop.run();
    let _ = stream.disconnect();
    if *failed.borrow() {
        return Err(pw::Error::CreationFailed);
    }
    Ok(())
}

/// Offers 32-bit RGB formats in shared memory. Without a DRM modifier in the
/// offer, compositors fall back to memfd buffers we can read directly.
fn format_params() -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let obj = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 60, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 240, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(obj))
        .expect("pod serialises")
        .0
        .into_inner()
}
