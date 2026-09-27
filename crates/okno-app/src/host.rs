//! "This Computer" page: running the host inside the app.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use okno_auth::{AllowList, Credentials};
use okno_core::desktop::{DesktopHandler, SERVICE_DESKTOP};
use okno_core::host::{ApprovalRequest, Approver, Host, HostEvent, HostSettings, SessionInfo};
use okno_desktop::{DesktopError, OpenOptions};
use okno_net::Identity;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::sync::broadcast;

use crate::app::App;
use crate::{Messages, SessionRow};

type Answer = tokio::sync::oneshot::Sender<bool>;

#[derive(Default)]
pub struct HostState {
    /// Connections waiting for the user's permission, shown one at a time.
    approvals: RefCell<std::collections::VecDeque<(u64, ApprovalRequest, Answer)>>,
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
    w.set_host_confirm(config.host.confirm_connections);
    w.set_host_new_user(if let Some(c) = &config.host.credentials { c.username.clone() } else { "okno".into() }.into());
    drop(config);
    refresh_login(app);
    show_status(app);
    show_settings(app);

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

    let weak = Rc::downgrade(app);
    w.on_host_confirm_changed(move |on| {
        let Some(app) = weak.upgrade() else { return };
        app.config.borrow_mut().host.confirm_connections = on;
        app.save_config();
        if let Some(host) = app.host.running.borrow().as_ref() {
            host.set_approver(on.then(approver));
        }
    });
    let weak = Rc::downgrade(app);
    w.on_approval_answered(move |allow| {
        let Some(app) = weak.upgrade() else { return };
        let front = app.host.approvals.borrow_mut().pop_front();
        if let Some((_, _, answer)) = front {
            let _ = answer.send(allow);
        }
        show_approval(&app);
    });
    let weak = Rc::downgrade(app);
    w.on_host_port_committed(move |text| {
        let Some(app) = weak.upgrade() else { return };
        match text.trim().parse::<u16>() {
            Ok(port) if port > 0 => {
                app.config.borrow_mut().host.port = port;
                settings_changed(&app);
            }
            _ => app.window.set_settings_error(messages(&app).invoke_bad_host_port()),
        }
    });
    let weak = Rc::downgrade(app);
    w.on_host_networks_committed(move |text| {
        let Some(app) = weak.upgrade() else { return };
        match AllowList::parse(text.split(',').filter(|s| !s.trim().is_empty())) {
            Ok(list) if !list.0.is_empty() => {
                app.config.borrow_mut().host.allowed_networks = list;
                settings_changed(&app);
            }
            Ok(_) => app.window.set_settings_error(messages(&app).invoke_bad_networks("—".into())),
            Err(e) => app.window.set_settings_error(messages(&app).invoke_bad_networks(e.to_string().into())),
        }
    });
    let weak = Rc::downgrade(app);
    w.on_host_choose_incoming(move || {
        let Some(app) = weak.upgrade() else { return };
        let title = messages(&app).invoke_choose_folder().to_string();
        let task = app.rt.spawn(async move { crate::picker::pick_folder(&title).await });
        let weak = Rc::downgrade(&app);
        let _ = slint::spawn_local(async move {
            let Ok(Some(dir)) = task.await else { return };
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().host.incoming_dir = Some(dir);
            settings_changed(&app);
        });
    });

    // Resume sharing if it was on when the app last ran.
    let config = app.config.borrow();
    if config.host.enabled && config.host.credentials.is_some() {
        drop(config);
        start(app);
    }
}

static NEXT_APPROVAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Asks the user through the approval dialog; gives up (denies) shortly
/// before the host's own timeout.
fn approver() -> Approver {
    Approver(Arc::new(|request: ApprovalRequest| {
        Box::pin(async move {
            let id = NEXT_APPROVAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let _ = slint::invoke_from_event_loop(move || crate::app::with_app(|app| ask(app, id, request, tx)));
            let wait = okno_core::host::APPROVAL_TIMEOUT - std::time::Duration::from_secs(2);
            match tokio::time::timeout(wait, rx).await {
                Ok(Ok(allow)) => allow,
                _ => {
                    let _ = slint::invoke_from_event_loop(move || crate::app::with_app(|app| withdraw(app, id)));
                    false
                }
            }
        })
    }))
}

fn ask(app: &App, id: u64, request: ApprovalRequest, answer: Answer) {
    let m = messages(app);
    crate::notify::show(
        &app.rt,
        &format!("okno-approval-{id}"),
        m.invoke_approval_title(request.device_name.as_str().into()).into(),
        m.invoke_approval_detail(request.peer.ip().to_string().into(), request.username.as_str().into()).into(),
    );
    let _ = app.window.show();
    app.window.set_page(1);
    app.host.approvals.borrow_mut().push_back((id, request, answer));
    show_approval(app);
}

fn withdraw(app: &App, id: u64) {
    app.host.approvals.borrow_mut().retain(|(i, _, _)| *i != id);
    show_approval(app);
}

fn show_approval(app: &App) {
    let w = &app.window;
    let approvals = app.host.approvals.borrow();
    let Some((_, req, _)) = approvals.front() else {
        w.set_approval_open(false);
        return;
    };
    let m = messages(app);
    w.set_approval_device(req.device_name.as_str().into());
    w.set_approval_detail(m.invoke_approval_detail(req.peer.ip().to_string().into(), req.username.as_str().into()));
    w.set_approval_fingerprint(req.fingerprint.display_short().into());
    w.set_approval_open(true);
}

fn show_settings(app: &App) {
    let config = app.config.borrow();
    let w = &app.window;
    let port = config.host.port.to_string();
    let networks = config.host.allowed_networks.0.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
    w.set_host_port(port.as_str().into());
    w.set_host_saved_port(port.into());
    w.set_host_networks(networks.as_str().into());
    w.set_host_saved_networks(networks.into());
    let incoming = config.host.incoming_dir.clone().unwrap_or_else(okno_core::files::FileService::default_incoming);
    w.set_host_incoming(incoming.to_string_lossy().as_ref().into());
    w.set_settings_error(SharedString::new());
}

/// Saves host settings and restarts a running host so they take effect.
fn settings_changed(app: &Rc<App>) {
    app.save_config();
    show_settings(app);
    if app.host.running.borrow().is_some() {
        stop(app);
        start(app);
    }
    app.toast(messages(app).invoke_settings_saved(), false);
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
            approver: settings_base.host.confirm_connections.then(approver),
        };
        let mut handler = DesktopHandler::new(opened.desktop);
        if let Some(dir) = settings_base.host.incoming_dir.clone() {
            handler = handler.with_incoming(dir);
        }
        let handler = Arc::new(handler);
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
        loop {
            let event = match events.recv().await {
                Ok(event) => Some(event),
                // A flood of refused connections can push session events
                // out of the queue; catch up from the host's own list.
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!("missed {missed} host events");
                    None
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            let Some(app) = weak.upgrade() else { return };
            if app.host.generation.get() != generation {
                return;
            }
            match event {
                Some(HostEvent::SessionOpened(info)) => {
                    notify_connected(&app, &info);
                    app.host.sessions.borrow_mut().push(info);
                }
                Some(HostEvent::SessionClosed { id, .. }) => app.host.sessions.borrow_mut().retain(|s| s.id != id),
                Some(_) => {}
                None => {
                    let Some(current) = app.host.running.borrow().as_ref().map(|h| h.sessions()) else { continue };
                    for info in &current {
                        if !app.host.sessions.borrow().iter().any(|s| s.id == info.id) {
                            notify_connected(&app, info);
                        }
                    }
                    *app.host.sessions.borrow_mut() = current;
                }
            }
            show_status(&app);
        }
    })
    .expect("event loop running");
}

/// Whoever sits at this computer must know it is being controlled.
fn notify_connected(app: &App, info: &SessionInfo) {
    let m = messages(app);
    crate::notify::show(
        &app.rt,
        &format!("okno-session-{}", info.id),
        m.invoke_connected_title(info.device_name.as_str().into()).into(),
        m.invoke_connected_body(info.peer.ip().to_string().into(), info.username.as_str().into()).into(),
    );
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
    for (_, _, answer) in app.host.approvals.borrow_mut().drain(..) {
        let _ = answer.send(false);
    }
    app.window.set_approval_open(false);
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
