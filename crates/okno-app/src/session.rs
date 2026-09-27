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
use crate::{Messages, SessionWindow, keys};

const MAX_FPS: u32 = 30;

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
}

impl SessionView {
    /// Opens the window and starts streaming the first display.
    /// `on_closed` runs on the UI thread once, when the session ends.
    pub fn open(
        session: Session,
        look: &Look,
        clipboard: LocalClipboard,
        on_closed: impl Fn(Option<String>) + 'static,
    ) -> Result<Rc<Self>, slint::PlatformError> {
        let window = SessionWindow::new()?;
        chrome::apply!(window, look);
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
        remote.request_video(primary as u32, MAX_FPS, 0);
        let files = crate::files_ui::install(&window, remote.files(), tokio::runtime::Handle::current(), &session_name);
        let remote = Rc::new(RefCell::new(Some(remote)));
        let display = Rc::new(Cell::new(primary as u32));

        // Session end: reported from a worker thread, handled here.
        let finish = {
            let weak = weak.clone();
            let remote = remote.clone();
            let on_closed = on_closed.clone();
            let closed_once = closed_once.clone();
            move |reason: Option<String>| {
                if closed_once.replace(true) {
                    return;
                }
                remote.borrow_mut().take();
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
            let weak = weak.clone();
            window.on_display_selected(move |i| {
                display.set(i as u32);
                if let Some(w) = weak.upgrade() {
                    w.set_has_frame(false);
                }
                if let Some(r) = remote.borrow().as_ref() {
                    r.request_video(i as u32, MAX_FPS, 0);
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
            window.window().on_winit_window_event(move |_, event| {
                if let Some(w) = weak.upgrade() {
                    chrome::observe!(w, cursor, event);
                }
                match event {
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
        Ok(Rc::new(Self { _window: window, remote, _poll_closed: poll_closed, _files: files }))
    }

    pub fn apply_look(&self, look: &Look) {
        chrome::apply!(self._window, look);
    }

    pub fn is_open(&self) -> bool {
        self.remote.borrow().is_some()
    }
}

fn close_remote(remote: &Rc<RefCell<Option<Remote>>>) {
    if let Some(r) = remote.borrow_mut().take() {
        tokio::spawn(r.close());
    }
}
