//! Application state on the UI thread and the Devices/Settings pages.
//!
//! Network work runs on the tokio runtime; results come back to the UI
//! thread through `slint::spawn_local` futures awaiting tokio join handles,
//! so all UI state lives in plain `Rc`/`RefCell`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use okno_auth::{TrustDecision, TrustStore};
use okno_core::client::{self, ClientError, Pending};
use okno_core::{Config, Endpoint, Store};
use okno_net::Identity;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::chrome::{self, Cursor, Look};
use crate::clipboard::LocalClipboard;
use crate::host::HostState;
use crate::session::SessionView;
use crate::{DeviceRow, DialogKind, MainWindow, Messages};

const DISCOVERY_TIME: Duration = Duration::from_secs(3);
const DISCOVERY_EVERY: Duration = Duration::from_secs(30);

pub struct App {
    pub rt: Handle,
    pub window: MainWindow,
    pub store: Store,
    pub config: RefCell<Config>,
    pub identity: Arc<Identity>,
    pub host: HostState,
    trust: RefCell<TrustStore>,
    pending: RefCell<Option<Pending>>,
    /// Bumped per connection attempt; cancelled attempts see a newer value.
    attempt: Cell<u64>,
    sessions: RefCell<Vec<Rc<SessionView>>>,
    toast_timer: slint::Timer,
    discovery_timer: slint::Timer,
    searching: Cell<bool>,
    look: RefCell<Look>,
    cursor: Cursor,
    clipboard: LocalClipboard,
}

thread_local! {
    /// The app, for callbacks posted to the UI thread from other threads.
    static APP: RefCell<std::rc::Weak<App>> = const { RefCell::new(std::rc::Weak::new()) };
}

impl App {
    pub fn new(rt: Handle) -> anyhow::Result<Rc<Self>> {
        let store = Store::open_default()?;
        let config = store.load_config()?;
        let identity = Arc::new(store.identity()?);
        let trust = store.load_trust().unwrap_or_else(|e| {
            tracing::warn!("ignoring unreadable trust store: {e}");
            TrustStore::default()
        });
        let window = MainWindow::new()?;
        crate::select_language();
        // Force a colour scheme (screenshots, testing): OKNO_COLOR_SCHEME=light|dark.
        match std::env::var("OKNO_COLOR_SCHEME").as_deref() {
            Ok("dark") => window.global::<crate::Theme>().set_dark(true),
            Ok("light") => window.global::<crate::Theme>().set_dark(false),
            _ => {}
        }
        window.set_version(env!("CARGO_PKG_VERSION").into());
        window.set_device_name(config.device_name.as_str().into());
        window.set_saved_device_name(config.device_name.as_str().into());
        window.set_login_user(
            if config.client.last_username.is_empty() { "okno".into() } else { config.client.last_username.clone() }
                .into(),
        );

        // Screenshot helper: open a given page directly.
        if let Some(page) = std::env::var("OKNO_UI_PAGE").ok().and_then(|p| p.parse().ok()) {
            window.set_page(page);
        }
        let app = Rc::new(Self {
            rt,
            window,
            store,
            config: RefCell::new(config),
            identity,
            host: HostState::default(),
            trust: RefCell::new(trust),
            pending: RefCell::default(),
            attempt: Cell::new(0),
            sessions: RefCell::default(),
            toast_timer: slint::Timer::default(),
            discovery_timer: slint::Timer::default(),
            searching: Cell::new(false),
            look: RefCell::default(),
            cursor: Cursor::default(),
            clipboard: LocalClipboard::new(),
        });
        APP.with(|a| *a.borrow_mut() = Rc::downgrade(&app));
        app.install_chrome();
        app.wire();
        crate::host::init(&app);
        app.show_recent();
        app.refresh();
        let weak = Rc::downgrade(&app);
        app.discovery_timer.start(slint::TimerMode::Repeated, DISCOVERY_EVERY, move || {
            if let Some(app) = weak.upgrade() {
                if app.window.get_page() == 0 {
                    app.refresh();
                }
            }
        });
        Ok(app)
    }

    pub fn run(&self) -> Result<(), slint::PlatformError> {
        self.window.run()
    }

    fn install_chrome(self: &Rc<Self>) {
        chrome::install!(self.window, self.cursor, || {
            let _ = slint::quit_event_loop();
        });
        let weak = self.window.as_weak();
        let cursor = self.cursor.clone();
        use slint::winit_030::WinitWindowAccessor as _;
        self.window.window().on_winit_window_event(move |_, event| {
            if let Some(w) = weak.upgrade() {
                chrome::observe!(w, cursor, event);
            }
            slint::winit_030::EventResult::Propagate
        });
        chrome::watch_look(&self.rt, |look| {
            APP.with(|a| {
                if let Some(app) = a.borrow().upgrade() {
                    app.set_look(look);
                }
            })
        });
    }

    fn set_look(&self, look: Look) {
        chrome::apply!(self.window, &look);
        for session in self.sessions.borrow().iter() {
            session.apply_look(&look);
        }
        *self.look.borrow_mut() = look;
    }

    fn messages(&self) -> Messages<'_> {
        self.window.global::<Messages>()
    }

    pub fn save_config(&self) {
        if let Err(e) = self.store.save_config(&self.config.borrow()) {
            self.toast(e.to_string().into(), true);
        }
    }

    pub fn toast(&self, text: SharedString, error: bool) {
        self.window.set_toast(text);
        self.window.set_toast_error(error);
        let weak = self.window.as_weak();
        self.toast_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(5), move || {
            if let Some(w) = weak.upgrade() {
                w.set_toast(SharedString::new());
            }
        });
    }

    fn wire(self: &Rc<Self>) {
        let w = &self.window;
        let weak = Rc::downgrade(self);
        w.on_refresh(move || {
            if let Some(app) = weak.upgrade() {
                app.refresh();
            }
        });
        let weak = Rc::downgrade(self);
        w.on_connect(move |endpoint| {
            if let Some(app) = weak.upgrade() {
                app.connect(endpoint.as_str());
            }
        });
        let weak = Rc::downgrade(self);
        w.on_forget(move |fingerprint| {
            let Some(app) = weak.upgrade() else { return };
            if let Some(fp) = okno_net::Fingerprint::from_hex(&fingerprint) {
                app.trust.borrow_mut().forget(&fp);
                app.save_trust();
                app.show_recent();
                app.toast(app.messages().invoke_forgotten(), false);
            }
        });
        let weak = Rc::downgrade(self);
        w.on_dialog_cancelled(move || {
            if let Some(app) = weak.upgrade() {
                app.cancel_connection();
            }
        });
        let weak = Rc::downgrade(self);
        w.on_trust_accepted(move || {
            if let Some(app) = weak.upgrade() {
                app.window.set_login_error(SharedString::new());
                app.window.set_dialog(DialogKind::Login);
            }
        });
        let weak = Rc::downgrade(self);
        w.on_login_submitted(move || {
            if let Some(app) = weak.upgrade() {
                app.login();
            }
        });
        w.on_open_url(|url| open_url(&url));
        let weak = Rc::downgrade(self);
        w.on_device_name_committed(move |name| {
            let Some(app) = weak.upgrade() else { return };
            let name = name.trim().to_owned();
            if name.is_empty() {
                return;
            }
            app.config.borrow_mut().device_name = name.clone();
            app.save_config();
            app.window.set_saved_device_name(name.into());
            app.toast(app.messages().invoke_name_saved(), false);
        });
    }

    fn save_trust(&self) {
        if let Err(e) = self.store.save_trust(&self.trust.borrow()) {
            self.toast(e.to_string().into(), true);
        }
    }

    // ---- Devices -------------------------------------------------------

    fn show_recent(&self) {
        let rows: Vec<DeviceRow> = self
            .trust
            .borrow()
            .devices
            .iter()
            .filter(|(_, d)| !d.endpoints.is_empty())
            .map(|(fp, d)| DeviceRow {
                name: d.name.as_str().into(),
                subtitle: d.endpoints.join(", ").into(),
                endpoint: d.endpoints[0].as_str().into(),
                fingerprint: fp.as_str().into(),
                known: true,
            })
            .collect();
        self.window.set_recent(ModelRc::new(VecModel::from(rows)));
    }

    fn refresh(self: &Rc<Self>) {
        if self.searching.replace(true) {
            return;
        }
        self.window.set_searching(true);
        let own = self.identity.fingerprint();
        let task = self.rt.spawn(okno_discovery::discover(DISCOVERY_TIME, Some(own)));
        let weak = Rc::downgrade(self);
        slint::spawn_local(async move {
            let peers = task.await.unwrap_or_default();
            let Some(app) = weak.upgrade() else { return };
            app.searching.set(false);
            app.window.set_searching(false);
            let trust = app.trust.borrow();
            let rows: Vec<DeviceRow> = peers
                .iter()
                .map(|p| {
                    let endpoint = Endpoint::from(p.addresses[0]).to_string();
                    DeviceRow {
                        name: p.name.as_str().into(),
                        subtitle: app
                            .messages()
                            .invoke_device_subtitle(endpoint.as_str().into(), os_label(&p.os).into()),
                        endpoint: endpoint.into(),
                        fingerprint: p.fingerprint.to_hex().into(),
                        known: trust.devices.contains_key(&p.fingerprint.to_hex()),
                    }
                })
                .collect();
            app.window.set_nearby(ModelRc::new(VecModel::from(rows)));
        })
        .expect("event loop running");
    }

    // ---- Connecting ----------------------------------------------------

    fn connect(self: &Rc<Self>, input: &str) {
        let endpoint: Endpoint = match input.parse() {
            Ok(e) => e,
            Err(_) => {
                self.toast(self.messages().invoke_invalid_address(input.into()), true);
                return;
            }
        };
        let attempt = self.attempt.get() + 1;
        self.attempt.set(attempt);
        self.pending.borrow_mut().take();
        self.window.set_dialog_host(endpoint.to_string().into());
        self.window.set_dialog(DialogKind::Connecting);

        let identity = self.identity.clone();
        let trust = self.trust.borrow().clone();
        let name = self.config.borrow().device_name.clone();
        let target = endpoint.clone();
        let task = self.rt.spawn(async move { client::open(&target, &identity, &trust, &name).await });
        let weak = Rc::downgrade(self);
        slint::spawn_local(async move {
            let result = task.await;
            let Some(app) = weak.upgrade() else { return };
            if app.attempt.get() != attempt {
                return; // cancelled
            }
            let pending = match result {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return app.connection_failed(&e, &endpoint.host),
                Err(e) => return app.connection_failed(&ClientError::Refused(e.to_string()), &endpoint.host),
            };
            let w = &app.window;
            w.set_dialog_host(pending.host.device_name.as_str().into());
            w.set_dialog_fingerprint(two_lines(&pending.fingerprint.display_short()).into());
            w.set_login_error(SharedString::new());
            w.set_login_password(SharedString::new());
            let dialog = match &pending.trust {
                TrustDecision::Trusted | TrustDecision::KnownElsewhere => DialogKind::Login,
                TrustDecision::New => DialogKind::TrustNew,
                TrustDecision::Changed { previous } => {
                    w.set_dialog_previous(previous.display_short().into());
                    DialogKind::TrustChanged
                }
            };
            *app.pending.borrow_mut() = Some(pending);
            w.set_dialog(dialog);
        })
        .expect("event loop running");
    }

    fn cancel_connection(&self) {
        self.attempt.set(self.attempt.get() + 1);
        self.pending.borrow_mut().take();
        self.window.set_login_busy(false);
        self.window.set_dialog(DialogKind::None);
    }

    fn connection_failed(&self, error: &ClientError, host: &str) {
        self.window.set_dialog(DialogKind::None);
        self.window.set_login_busy(false);
        self.pending.borrow_mut().take();
        self.toast(self.error_text(error, host), true);
    }

    fn error_text(&self, error: &ClientError, host: &str) -> SharedString {
        let m = self.messages();
        match error {
            ClientError::Resolve(h) => m.invoke_cannot_resolve(h.as_str().into()),
            ClientError::Connect { .. } => m.invoke_unreachable(host.into()),
            ClientError::Timeout => m.invoke_timeout(),
            ClientError::Version(_) => m.invoke_version(),
            ClientError::Refused(r) => m.invoke_refused(r.as_str().into()),
            ClientError::Busy => m.invoke_busy(),
            ClientError::NotConfigured => m.invoke_not_configured(),
            ClientError::BadCredentials { .. } => m.invoke_bad_credentials(),
            ClientError::Throttled { retry_after } => m.invoke_throttled(retry_after.as_secs().max(1) as i32),
            ClientError::Net(e) => m.invoke_network(e.to_string().into()),
            ClientError::Protocol => m.invoke_protocol(),
        }
    }

    fn login(self: &Rc<Self>) {
        let Some(mut pending) = self.pending.borrow_mut().take() else { return };
        let w = &self.window;
        let user = match w.get_login_user().trim() {
            "" => "okno".to_owned(),
            u => u.to_owned(),
        };
        let password = w.get_login_password().to_string();
        w.set_login_busy(true);
        w.set_login_error(SharedString::new());
        let attempt = self.attempt.get();
        let task_user = user.clone();
        let task = self.rt.spawn(async move {
            let result = pending.login(&task_user, &password).await;
            (pending, result)
        });
        let weak = Rc::downgrade(self);
        slint::spawn_local(async move {
            let joined = task.await;
            let Some(app) = weak.upgrade() else { return };
            if app.attempt.get() != attempt {
                return;
            }
            app.window.set_login_busy(false);
            let (pending, result) = match joined {
                Ok(x) => x,
                Err(e) => return app.connection_failed(&ClientError::Refused(e.to_string()), ""),
            };
            match result {
                Ok(()) => app.logged_in(pending, user),
                Err(e @ (ClientError::BadCredentials { .. } | ClientError::Throttled { .. } | ClientError::Busy)) => {
                    let host = pending.host.device_name.clone();
                    app.window.set_login_error(app.error_text(&e, &host));
                    app.window.set_login_password(SharedString::new());
                    *app.pending.borrow_mut() = Some(pending);
                }
                Err(e) => {
                    let host = pending.host.device_name.clone();
                    app.connection_failed(&e, &host);
                }
            }
        })
        .expect("event loop running");
    }

    fn logged_in(self: &Rc<Self>, pending: Pending, user: String) {
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        self.trust.borrow_mut().trust(
            &pending.endpoint.to_string(),
            &pending.fingerprint,
            &pending.host.device_name,
            now,
        );
        self.save_trust();
        self.show_recent();
        self.config.borrow_mut().client.last_username = user;
        self.save_config();
        self.window.set_login_password(SharedString::new());
        self.window.set_dialog(DialogKind::None);

        let session = pending.into_session();
        let weak = Rc::downgrade(self);
        let opened = SessionView::open(session, &self.look.borrow(), self.clipboard.clone(), move |reason| {
            let Some(app) = weak.upgrade() else { return };
            app.sessions.borrow_mut().retain(|s| s.is_open());
            let text = match reason {
                Some(r) => app.messages().invoke_session_closed(r.into()),
                None => app.messages().invoke_session_ended(),
            };
            app.toast(text, false);
        });
        match opened {
            Ok(view) => self.sessions.borrow_mut().push(view),
            Err(e) => self.toast(e.to_string().into(), true),
        }
    }
}

/// Opens a link in the default browser.
fn open_url(url: &str) {
    let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
    match std::process::Command::new(program).arg(url).spawn() {
        // Reap the helper so it does not linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::warn!("cannot open {url}: {e}"),
    }
}

/// "AAAA BBBB … HHHH" as two lines of four groups, easier to compare.
fn two_lines(fingerprint: &str) -> String {
    let groups: Vec<&str> = fingerprint.split(' ').collect();
    let (first, second) = groups.split_at(groups.len().div_ceil(2));
    format!("{}\n{}", first.join(" "), second.join(" "))
}

fn os_label(os: &str) -> &str {
    match os {
        "linux" => "Linux",
        "windows" => "Windows",
        other => other,
    }
}
