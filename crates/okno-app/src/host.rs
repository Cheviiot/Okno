//! "This Computer" page: running the host inside the app.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use okno_auth::Credentials;
use okno_core::desktop::{DesktopHandler, SERVICE_DESKTOP};
use okno_core::host::{Host, HostEvent, HostSettings, SessionInfo};
use okno_desktop::{DesktopError, OpenOptions};
use okno_net::Identity;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::app::App;
use crate::{Messages, SessionRow};

#[derive(Default)]
pub struct HostState {
    running: RefCell<Option<Arc<Host>>>,
    /// Bumped on every start/stop so stale async results are ignored.
    generation: Cell<u64>,
    sessions: RefCell<Vec<SessionInfo>>,
}

pub fn init(app: &Rc<App>) {
    let w = &app.window;
    let config = app.config.borrow();
    w.set_host_fingerprint(app.identity.fingerprint().display_short().into());
    w.set_host_discoverable(config.host.discoverable);
    w.set_host_new_user(if let Some(c) = &config.host.credentials { c.username.clone() } else { "okno".into() }.into());
    drop(config);
    refresh_login(app);
    show_status(app);

    let weak = Rc::downgrade(app);
    w.on_host_toggle(move |on| {
        let Some(app) = weak.upgrade() else { return };
        if on { start(&app) } else { stop(&app) }
    });
    let weak = Rc::downgrade(app);
    w.on_host_set_login(move || {
        if let Some(app) = weak.upgrade() {
            set_login(&app);
        }
    });
    let weak = Rc::downgrade(app);
    w.on_host_disconnect(move |id| {
        let Some(app) = weak.upgrade() else { return };
        if let Some(host) = app.host.running.borrow().as_ref() {
            host.disconnect(id as u64);
        }
    });
    let weak = Rc::downgrade(app);
    w.on_host_discoverable_changed(move |on| {
        let Some(app) = weak.upgrade() else { return };
        let name = {
            let mut config = app.config.borrow_mut();
            config.host.discoverable = on;
            config.device_name.clone()
        };
        app.save_config();
        if let Some(host) = app.host.running.borrow().clone() {
            app.rt.spawn(async move { host.set_discoverable(on, &name).await });
        }
    });

    // Resume sharing if it was on when the app last ran.
    let config = app.config.borrow();
    if config.host.enabled && config.host.credentials.is_some() {
        drop(config);
        start(app);
    }
}

fn refresh_login(app: &App) {
    let config = app.config.borrow();
    app.window.set_host_has_login(config.host.credentials.is_some());
    if let Some(c) = &config.host.credentials {
        app.window.set_host_login_user(c.username.as_str().into());
    }
}

fn messages(app: &App) -> Messages<'_> {
    app.window.global::<Messages>()
}

fn show_status(app: &App) {
    let w = &app.window;
    let host = app.host.running.borrow();
    w.set_host_running(host.is_some());
    let status = match host.as_ref() {
        None if w.get_host_has_login() => messages(app).invoke_host_off(),
        None => messages(app).invoke_host_no_login(),
        Some(_) if !app.host.sessions.borrow().is_empty() => {
            messages(app).invoke_host_sessions(app.host.sessions.borrow().len() as i32)
        }
        Some(h) => messages(app).invoke_host_listening(h.local_addrs().first().map(|a| a.port()).unwrap_or(0) as i32),
    };
    w.set_host_status(status);
    let addresses = match host.as_ref() {
        Some(h) => local_addresses(h.local_addrs().first().map(|a| a.port()).unwrap_or(0)),
        None => String::new(),
    };
    w.set_host_addresses(addresses.into());
    let rows: Vec<SessionRow> = app
        .host
        .sessions
        .borrow()
        .iter()
        .map(|s| SessionRow {
            id: s.id as i32,
            title: s.device_name.as_str().into(),
            subtitle: messages(app).invoke_session_from(s.peer.ip().to_string().into(), s.username.as_str().into()),
        })
        .collect();
    w.set_host_sessions(ModelRc::new(VecModel::from(rows)));
}

/// IPv4 addresses of this machine's interfaces, as people would type them.
fn local_addresses(port: u16) -> String {
    let mut ips: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|iface| match iface.addr {
            if_addrs::IfAddr::V4(v4) if !iface.is_loopback() => Some(v4.ip),
            _ => None,
        })
        .collect();
    ips.sort();
    ips.dedup();
    ips.iter()
        .map(|ip| if port == okno_proto::DEFAULT_PORT { ip.to_string() } else { format!("{ip}:{port}") })
        .collect::<Vec<_>>()
        .join(", ")
}

fn start(app: &Rc<App>) {
    let (credentials, settings_base, token) = {
        let config = app.config.borrow();
        let Some(credentials) = config.host.credentials.clone() else {
            app.window.set_host_running(false);
            return;
        };
        (credentials, config.clone(), config.host.portal_restore_token.clone())
    };
    let generation = app.host.generation.get() + 1;
    app.host.generation.set(generation);
    app.window.set_host_starting(true);
    app.window.set_host_status(messages(app).invoke_host_permission());

    let weak: Weak<App> = Rc::downgrade(app);
    let rt = app.rt.clone();
    let identity = Identity::from_private(app.identity.private_key()).expect("valid key");
    slint::spawn_local(async move {
        let opened = rt.spawn(okno_desktop::open(OpenOptions { restore_token: token })).await;
        let Some(app) = weak.upgrade() else { return };
        if app.host.generation.get() != generation {
            return;
        }
        let opened = match opened {
            Ok(Ok(opened)) => opened,
            Ok(Err(e)) => return failed(&app, e),
            Err(e) => return failed(&app, DesktopError::Capture(e.to_string())),
        };
        if opened.restore_token.is_some() {
            app.config.borrow_mut().host.portal_restore_token = opened.restore_token.clone();
        }
        let settings = HostSettings {
            device_name: settings_base.device_name.clone(),
            port: settings_base.host.port,
            listen: settings_base.host.listen.clone(),
            allowed_networks: settings_base.host.allowed_networks.clone(),
            credentials,
            discoverable: settings_base.host.discoverable,
            services: vec![SERVICE_DESKTOP.into()],
        };
        let handler = Arc::new(DesktopHandler::new(opened.desktop));
        drop(app);
        let started = rt.spawn(Host::start(identity, settings, handler)).await;
        let Some(app) = weak.upgrade() else { return };
        if app.host.generation.get() != generation {
            return;
        }
        let host = match started {
            Ok(Ok(host)) => Arc::new(host),
            Ok(Err(e)) => return failed(&app, DesktopError::Capture(e.to_string())),
            Err(e) => return failed(&app, DesktopError::Capture(e.to_string())),
        };
        let mut events = host.subscribe();
        *app.host.running.borrow_mut() = Some(host);
        app.host.sessions.borrow_mut().clear();
        app.config.borrow_mut().host.enabled = true;
        app.save_config();
        app.window.set_host_starting(false);
        show_status(&app);
        drop(app);

        // Follow host events until it stops.
        while let Ok(event) = events.recv().await {
            let Some(app) = weak.upgrade() else { return };
            if app.host.generation.get() != generation {
                return;
            }
            match event {
                HostEvent::SessionOpened(info) => app.host.sessions.borrow_mut().push(info),
                HostEvent::SessionClosed { id, .. } => app.host.sessions.borrow_mut().retain(|s| s.id != id),
                _ => {}
            }
            show_status(&app);
        }
    })
    .expect("event loop running");
}

fn failed(app: &App, error: DesktopError) {
    app.window.set_host_starting(false);
    app.window.set_host_running(false);
    let text = match error {
        DesktopError::Denied => messages(app).invoke_host_denied(),
        other => messages(app).invoke_host_failed(other.to_string().into()),
    };
    app.window.set_host_status(text.clone());
    app.toast(text, true);
}

fn stop(app: &App) {
    app.host.generation.set(app.host.generation.get() + 1);
    app.host.running.borrow_mut().take();
    app.host.sessions.borrow_mut().clear();
    app.window.set_host_starting(false);
    app.config.borrow_mut().host.enabled = false;
    app.save_config();
    show_status(app);
}

fn set_login(app: &Rc<App>) {
    let w = &app.window;
    let user = w.get_host_new_user().trim().to_owned();
    let user = if user.is_empty() { "okno".to_owned() } else { user };
    let password = w.get_host_new_password().to_string();
    if password != w.get_host_new_repeat().as_str() {
        w.set_host_login_error(messages(app).invoke_passwords_differ());
        return;
    }
    w.set_host_login_error(SharedString::new());
    let weak = Rc::downgrade(app);
    let rt = app.rt.clone();
    slint::spawn_local(async move {
        // Argon2 takes a fraction of a second; keep the UI responsive.
        let result = rt.spawn_blocking(move || Credentials::new(&user, &password)).await;
        let Some(app) = weak.upgrade() else { return };
        let w = &app.window;
        match result {
            Ok(Ok(credentials)) => {
                if let Some(host) = app.host.running.borrow().as_ref() {
                    host.set_credentials(credentials.clone());
                }
                app.config.borrow_mut().host.credentials = Some(credentials);
                app.save_config();
                w.set_host_new_password(SharedString::new());
                w.set_host_new_repeat(SharedString::new());
                refresh_login(&app);
                show_status(&app);
                app.toast(messages(&app).invoke_login_saved(), false);
            }
            Ok(Err(e)) => w.set_host_login_error(messages(&app).invoke_bad_login(e.to_string().into())),
            Err(e) => w.set_host_login_error(e.to_string().into()),
        }
    })
    .expect("event loop running");
}
