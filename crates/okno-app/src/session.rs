//! The window of one remote session.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use okno_core::client::Session;
use okno_core::remote::{Remote, RemoteEvent};
use okno_proto::input_event::Event;
use okno_proto::{InputEvent, KeyEvent, MouseButton, PointerButton, PointerMotion, PointerScroll};
use slint::winit_030::winit::event::{ElementState, WindowEvent};
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use crate::chrome::{self, Cursor, Look};
use crate::clipboard::LocalClipboard;
use crate::terminal_ui::TerminalView;
use crate::{ForwardRow, Messages, SessionWindow, keys};
use okno_core::tunnel::Forward;

/// Picture presets of the session menu: (frames per second, kbit/s).
const PRESETS: [(u32, u32); 3] = [(30, 8_000), (30, 20_000), (60, 6_000)];

struct Frame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Newest decoded frame, handed from the decoder thread to the UI thread.
/// Only one repaint is queued at a time, so a slow UI skips frames instead
/// of piling up work.
#[derive(Default)]
struct FrameMailbox {
    frame: Mutex<Option<Frame>>,
    queued: AtomicBool,
}

pub struct SessionView {
    /// Strong handle that keeps the window alive.
    _window: SessionWindow,
    remote: Rc<RefCell<Option<Remote>>>,
    /// Checks for the end of the session reported by worker threads.
    _poll_closed: slint::Timer,
    _files: crate::files_ui::FilesUi,
    terminals: Rc<RefCell<Vec<Rc<TerminalView>>>>,
    _forwards: Rc<RefCell<Vec<Forward>>>,
    _sound: Rc<RefCell<Option<okno_audio::Playback>>>,
    _stats_timer: slint::Timer,
}

/// Asks for a real display, or for the host's desktop on a virtual screen.
fn request(remote: &Remote, display: u32, virtual_size: Option<(u32, u32)>, (fps, kbps): (u32, u32)) {
    match virtual_size {
        Some((width, height)) => remote.request_virtual_video(width, height, fps, kbps),
        None => remote.request_video(display, fps, kbps),
    }
}

/// How the remote screen fits the window, and where to remember a change.
pub struct Scaling {
    pub scale_to_window: bool,
    pub remember: Box<dyn Fn(bool)>,
}

impl SessionView {
    /// Opens the window and starts streaming the first display.
    /// `on_closed` runs on the UI thread once, when the session ends.
    pub fn open(
        session: Session,
        look: &Look,
        clipboard: LocalClipboard,
        scaling: Scaling,
        on_closed: impl Fn(Option<String>) + 'static,
    ) -> Result<Rc<Self>, slint::PlatformError> {
        let window = SessionWindow::new()?;
        chrome::apply!(window, look);
        window.set_scale_to_window(scaling.scale_to_window);
        window.on_scaling_changed(move |on| (scaling.remember)(on));
        let session_name = session.host.device_name.clone();
        let cursor = Cursor::default();
        let messages = window.global::<Messages>();
        window.set_session_title(SharedString::from(format!("{} — Okno", session.host.device_name)));
        window.set_status(messages.invoke_video_waiting());
        let displays: Vec<SharedString> = session
            .host_info
            .displays
            .iter()
            .enumerate()
            .map(|(i, d)| {
                messages.invoke_display_name(i as i32 + 1, d.name.as_str().into(), d.width as i32, d.height as i32)
            })
            .collect();
        // Virtual screens the host can switch to follow the real displays.
        let virtual_modes: Vec<(u32, u32)> =
            session.host_info.virtual_modes.iter().map(|m| (m.width, m.height)).collect();
        let displays: Vec<SharedString> = displays
            .into_iter()
            .chain(virtual_modes.iter().map(|&(w, h)| messages.invoke_virtual_display_name(w as i32, h as i32)))
            .collect();
        let real_displays = session.host_info.displays.len();
        window.set_displays(ModelRc::new(VecModel::from(displays)));
        let primary = session.host_info.displays.iter().position(|d| d.primary).unwrap_or(0);
        window.set_display(primary as i32);

        let mailbox = Arc::new(FrameMailbox::default());
        let weak = window.as_weak();
        let on_closed = Rc::new(on_closed);
        let closed_once = Rc::new(Cell::new(false));

        // Last text exchanged either way, so a text is never echoed back.
        let synced: Arc<Mutex<Option<String>>> = Arc::default();
        let sink_synced = synced.clone();
        let sink_clipboard = clipboard.clone();
        let sink_mailbox = mailbox.clone();
        let sink_window = weak.clone();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel::<Option<String>>();
        let remote = session.run(Arc::new(move |event| match event {
            RemoteEvent::Frame { width, height, rgba, .. } => {
                *sink_mailbox.frame.lock().unwrap() = Some(Frame { width, height, rgba });
                if !sink_mailbox.queued.swap(true, Ordering::AcqRel) {
                    let mailbox = sink_mailbox.clone();
                    let _ = sink_window.upgrade_in_event_loop(move |w| {
                        mailbox.queued.store(false, Ordering::Release);
                        if let Some(f) = mailbox.frame.lock().unwrap().take() {
                            let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&f.rgba, f.width, f.height);
                            w.set_frame(Image::from_rgba8(buffer));
                            w.set_has_frame(true);
                        }
                    });
                }
            }
            RemoteEvent::Closed(reason) => {
                let _ = closed_tx.send(reason);
                let _ = sink_window.upgrade_in_event_loop(|_| {});
            }
            RemoteEvent::Clipboard(text) => {
                *sink_synced.lock().unwrap() = Some(text.clone());
                sink_clipboard.set(text);
            }
            RemoteEvent::Error(e) => tracing::warn!("host reported: {e}"),
            _ => {}
        }));
        let quality = Rc::new(Cell::new(0usize));
        let (fps, kbps) = PRESETS[0];
        remote.request_video(primary as u32, fps, kbps);
        // Sound: on by default; OKNO_NO_SOUND=1 disables it (tests).
        let sound: Rc<RefCell<Option<okno_audio::Playback>>> = Rc::default();
        let start_sound = {
            let sound = sound.clone();
            move |remote: &Remote| -> bool {
                // Test runs never touch the real audio devices.
                if std::env::var_os("OKNO_NO_SOUND").is_some() {
                    return false;
                }
                let ring = Arc::new(okno_audio::SampleRing::default());
                match okno_audio::play(ring.clone()).and_then(|p| remote.start_audio(ring).map(|_| p)) {
                    Ok(playback) => {
                        *sound.borrow_mut() = Some(playback);
                        true
                    }
                    Err(e) => {
                        tracing::warn!("sound unavailable: {e}");
                        false
                    }
                }
            }
        };
        let sound_on = start_sound(&remote);
        window.set_sound_on(sound_on);
        let files = crate::files_ui::install(&window, remote.files(), tokio::runtime::Handle::current(), &session_name);
        let terminals: Rc<RefCell<Vec<Rc<TerminalView>>>> = Rc::default();
        let forwards: Rc<RefCell<Vec<Forward>>> = Rc::default();
        let tools = (remote.terminals(), remote.tunnels());
        let remote = Rc::new(RefCell::new(Some(remote)));
        let display = Rc::new(Cell::new(primary as u32));
        // Set while the host shows its desktop on a virtual screen.
        let virtual_size: Rc<Cell<Option<(u32, u32)>>> = Rc::default();

        // Session end: reported from a worker thread, handled here.
        let finish = {
            let sound = sound.clone();
            let terminals = terminals.clone();
            let forwards = forwards.clone();
            let weak = weak.clone();
            let remote = remote.clone();
            let on_closed = on_closed.clone();
            let closed_once = closed_once.clone();
            move |reason: Option<String>| {
                if closed_once.replace(true) {
                    return;
                }
                remote.borrow_mut().take();
                for t in terminals.borrow_mut().drain(..) {
                    t.close();
                }
                forwards.borrow_mut().clear();
                sound.borrow_mut().take();
                if let Some(w) = weak.upgrade() {
                    let _ = w.hide();
                }
                on_closed(reason);
            }
        };
        let poll_closed = slint::Timer::default();
        {
            let finish = finish.clone();
            poll_closed.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(200), move || {
                if let Ok(reason) = closed_rx.try_recv() {
                    finish(reason);
                }
            });
        }

        let send = {
            let remote = remote.clone();
            move |event: Event| {
                if let Some(r) = remote.borrow().as_ref() {
                    r.send_input(InputEvent { event: Some(event) });
                }
            }
        };
        let send = Rc::new(send);

        {
            let send = send.clone();
            let display = display.clone();
            window.on_pointer_moved(move |x, y| {
                if (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y) {
                    send(Event::Motion(PointerMotion { display: display.get(), x: x as f64, y: y as f64 }));
                }
            });
        }
        {
            let send = send.clone();
            window.on_pointer_button(move |button, pressed| {
                let button = match button {
                    1 => MouseButton::Left,
                    2 => MouseButton::Right,
                    3 => MouseButton::Middle,
                    4 => MouseButton::Back,
                    5 => MouseButton::Forward,
                    _ => return,
                };
                send(Event::Button(PointerButton { button: button as i32, pressed }));
            });
        }
        {
            let send = send.clone();
            window.on_scrolled(move |dx, dy| {
                // Slint: positive delta scrolls content down (wheel up).
                // Protocol: positive scrolls down.
                send(Event::Scroll(PointerScroll { dx: -dx as f64, dy: -dy as f64, steps_x: 0, steps_y: 0 }));
            });
        }
        {
            let remote = remote.clone();
            let display = display.clone();
            let virtual_size = virtual_size.clone();
            let quality = quality.clone();
            window.on_quality_selected(move |i| {
                quality.set((i as usize).min(PRESETS.len() - 1));
                if let Some(r) = remote.borrow().as_ref() {
                    request(r, display.get(), virtual_size.get(), PRESETS[quality.get()]);
                }
            });
        }
        {
            let remote = remote.clone();
            let display = display.clone();
            let virtual_size = virtual_size.clone();
            let quality = quality.clone();
            let weak = weak.clone();
            window.on_display_selected(move |i| {
                let i = i.max(0) as usize;
                match virtual_modes.get(i.wrapping_sub(real_displays)) {
                    Some(&size) if i >= real_displays => virtual_size.set(Some(size)),
                    _ => {
                        display.set(i as u32);
                        virtual_size.set(None);
                    }
                }
                if let Some(w) = weak.upgrade() {
                    w.set_has_frame(false);
                }
                if let Some(r) = remote.borrow().as_ref() {
                    request(r, display.get(), virtual_size.get(), PRESETS[quality.get()]);
                }
            });
        }
        {
            let weak = weak.clone();
            window.on_toggle_fullscreen(move || {
                if let Some(w) = weak.upgrade() {
                    w.set_fullscreen(!w.get_fullscreen());
                }
            });
        }
        {
            let finish = finish.clone();
            let remote = remote.clone();
            window.on_disconnect(move || {
                close_remote(&remote);
                finish(None);
            });
        }
        {
            let finish = finish.clone();
            let remote = remote.clone();
            chrome::install!(window, cursor, move || {
                close_remote(&remote);
                finish(None);
            });
        }
        {
            let finish = finish.clone();
            let remote = remote.clone();
            window.window().on_close_requested(move || {
                close_remote(&remote);
                finish(None);
                slint::CloseRequestResponse::HideWindow
            });
        }

        // Keyboard: physical keys straight from winit. Slint never sees
        // them, so shortcuts go to the remote machine.
        let pressed: Rc<RefCell<BTreeSet<u32>>> = Rc::default();
        {
            let send = send.clone();
            let weak = weak.clone();
            let cursor = cursor.clone();
            let remote = remote.clone();
            let clipboard = clipboard.clone();
            window.window().on_winit_window_event(move |_, event| {
                if let Some(w) = weak.upgrade() {
                    chrome::observe!(w, cursor, event);
                }
                match event {
                    // Sheets and menus of the session window take the keyboard
                    // themselves; everything else goes to the remote machine.
                    WindowEvent::KeyboardInput { .. }
                        if weak
                            .upgrade()
                            .is_some_and(|w| w.get_files_open() || w.get_ports_open() || w.get_tools_open()) =>
                    {
                        EventResult::Propagate
                    }
                    WindowEvent::KeyboardInput { event, .. } => {
                        // F11 toggles full screen locally.
                        if event.state == ElementState::Pressed
                            && event.physical_key
                                == slint::winit_030::winit::keyboard::PhysicalKey::Code(
                                    slint::winit_030::winit::keyboard::KeyCode::F11,
                                )
                        {
                            if !event.repeat {
                                if let Some(w) = weak.upgrade() {
                                    w.set_fullscreen(!w.get_fullscreen());
                                }
                            }
                            return EventResult::PreventDefault;
                        }
                        // The remote system repeats held keys itself.
                        if event.repeat {
                            return EventResult::PreventDefault;
                        }
                        if let Some(code) = keys::evdev_code(event.physical_key) {
                            let down = event.state == ElementState::Pressed;
                            let changed = if down {
                                pressed.borrow_mut().insert(code)
                            } else {
                                pressed.borrow_mut().remove(&code)
                            };
                            if changed || down {
                                send(Event::Key(KeyEvent { evdev_code: code, pressed: down }));
                            }
                        }
                        EventResult::PreventDefault
                    }
                    // Keys held while focus leaves (Alt+Tab) would stay stuck on
                    // the remote side.
                    // Coming back to the session: hand over what was copied
                    // locally meanwhile.
                    WindowEvent::Focused(true) => {
                        if let Some(text) = clipboard.get() {
                            let mut last = synced.lock().unwrap();
                            if last.as_ref() != Some(&text) && text.len() <= okno_desktop::MAX_CLIPBOARD {
                                if let Some(r) = remote.borrow().as_ref() {
                                    r.send_clipboard(text.clone());
                                }
                                *last = Some(text);
                            }
                        }
                        EventResult::Propagate
                    }
                    WindowEvent::Focused(false) => {
                        for code in std::mem::take(&mut *pressed.borrow_mut()) {
                            send(Event::Key(KeyEvent { evdev_code: code, pressed: false }));
                        }
                        EventResult::Propagate
                    }
                    _ => EventResult::Propagate,
                }
            });
        }

        window.show()?;
        {
            let remote = remote.clone();
            let weak = window.as_weak();
            let sound = sound.clone();
            window.on_sound_toggled(move || {
                let Some(w) = weak.upgrade() else { return };
                let guard = remote.borrow();
                let Some(r) = guard.as_ref() else { return };
                if sound.borrow().is_some() {
                    r.stop_audio();
                    sound.borrow_mut().take();
                    w.set_sound_on(false);
                } else {
                    w.set_sound_on(start_sound(r));
                }
            });
        }
        // Terminal.
        {
            let (term_client, _) = &tools;
            let term_client = term_client.clone();
            let terminals = terminals.clone();
            let look = look.clone();
            let clipboard = clipboard.clone();
            let host = session_name.clone();
            window.on_open_terminal(move || {
                terminals.borrow_mut().retain(|t| t.is_open());
                match TerminalView::open(&term_client, &host, &look, clipboard.clone()) {
                    Ok(view) => terminals.borrow_mut().push(view),
                    Err(e) => tracing::warn!("cannot open terminal window: {e}"),
                }
            });
        }
        // Port forwarding.
        {
            let (_, tunnels) = tools;
            let weak = window.as_weak();
            let forwards = forwards.clone();
            let host = session_name.clone();
            let rt = tokio::runtime::Handle::current();
            window.on_add_forward(move || {
                let Some(w) = weak.upgrade() else { return };
                let m = w.global::<Messages>();
                let port_text = w.get_forward_local().trim().to_owned();
                let port: u16 = if port_text.is_empty() {
                    0
                } else {
                    match port_text.parse() {
                        Ok(p) if p > 0 => p,
                        _ => return w.set_forward_error(m.invoke_bad_port()),
                    }
                };
                let target = w.get_forward_target().trim().to_owned();
                let valid_target = target
                    .rsplit_once(':')
                    .is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
                if !valid_target {
                    return w.set_forward_error(m.invoke_bad_target());
                }
                w.set_forward_error(slint::SharedString::new());
                let local = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                let task = {
                    let tunnels = tunnels.clone();
                    let target = target.clone();
                    rt.spawn(async move { tunnels.forward(local, target).await })
                };
                let weak = weak.clone();
                let forwards = forwards.clone();
                let host = host.clone();
                let _ = slint::spawn_local(async move {
                    let result = task.await;
                    let Some(w) = weak.upgrade() else { return };
                    let m = w.global::<Messages>();
                    match result {
                        Ok(Ok(forward)) => {
                            forwards.borrow_mut().push(forward);
                            w.set_forward_local(slint::SharedString::new());
                            w.set_forward_target(slint::SharedString::new());
                            show_forwards(&w, &forwards.borrow(), &host);
                        }
                        Ok(Err(e)) => w.set_forward_error(m.invoke_forward_failed(e.to_string().into())),
                        Err(e) => w.set_forward_error(m.invoke_forward_failed(e.to_string().into())),
                    }
                });
            });
        }
        {
            let weak = window.as_weak();
            let forwards = forwards.clone();
            let host = session_name.clone();
            window.on_remove_forward(move |i| {
                let Some(w) = weak.upgrade() else { return };
                let mut list = forwards.borrow_mut();
                if (i as usize) < list.len() {
                    list.remove(i as usize);
                }
                show_forwards(&w, &list, &host);
            });
        }

        // Statistics overlay, refreshed every second while shown.
        let stats_timer = slint::Timer::default();
        {
            let remote = remote.clone();
            let weak = window.as_weak();
            let last = Cell::new(okno_core::remote::Stats::default());
            let last_at = Cell::new(std::time::Instant::now());
            stats_timer.start(slint::TimerMode::Repeated, std::time::Duration::from_secs(1), move || {
                let Some(w) = weak.upgrade() else { return };
                let Some(now) = remote.borrow().as_ref().map(|r| r.stats()) else { return };
                let secs = last_at.replace(std::time::Instant::now()).elapsed().as_secs_f64().max(0.001);
                let before = last.replace(now);
                if !w.get_stats_visible() {
                    return;
                }
                let fps = ((now.frames - before.frames) as f64 / secs).round() as i32;
                let mbit = (now.video_bytes - before.video_bytes) as f64 * 8.0 / secs / 1e6;
                let mbit = format!("{mbit:.1}");
                let mbit = if crate::uses_decimal_comma() { mbit.replace('.', ",") } else { mbit };
                let rtt = now
                    .rtt
                    .map(|d| {
                        let ms = d.as_secs_f64() * 1000.0;
                        let text = if ms < 10.0 { format!("{ms:.1}") } else { format!("{ms:.0}") };
                        if crate::uses_decimal_comma() { text.replace('.', ",") } else { text }
                    })
                    .unwrap_or_else(|| "—".into());
                w.set_stats_text(w.global::<Messages>().invoke_stats(fps, mbit.into(), rtt.into()));
            });
        }

        Ok(Rc::new(Self {
            _window: window,
            remote,
            _poll_closed: poll_closed,
            _files: files,
            terminals,
            _forwards: forwards,
            _sound: sound,
            _stats_timer: stats_timer,
        }))
    }

    pub fn apply_look(&self, look: &Look) {
        chrome::apply!(self._window, look);
        for t in self.terminals.borrow().iter() {
            t.apply_look(look);
        }
    }

    pub fn is_open(&self) -> bool {
        self.remote.borrow().is_some()
    }
}

fn show_forwards(window: &SessionWindow, forwards: &[Forward], host: &str) {
    let m = window.global::<Messages>();
    let rows: Vec<ForwardRow> = forwards
        .iter()
        .map(|f| ForwardRow {
            title: m.invoke_forward_title(f.local_addr().port() as i32, f.target().into()),
            subtitle: m.invoke_forward_subtitle(host.into()),
        })
        .collect();
    window.set_forwards(ModelRc::new(VecModel::from(rows)));
}

fn close_remote(remote: &Rc<RefCell<Option<Remote>>>) {
    if let Some(r) = remote.borrow_mut().take() {
        tokio::spawn(r.close());
    }
}
