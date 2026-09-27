//! The side that is being controlled.
//!
//! [`Host::start`] listens on the configured addresses. Every connection goes
//! through: allowlist → Noise handshake → `Hello` exchange → `Login` (with
//! throttling) → `HostInfo`, and is then handed to a [`SessionHandler`].

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use okno_auth::{AllowList, Credentials, LoginThrottle, ThrottleDecision};
use okno_discovery::{Announcer, HostAnnouncement};
use okno_net::{Fingerprint, Identity, Receiver, Sender};
use okno_proto::envelope::Msg;
use okno_proto::{Close, Display, ErrorMsg, Hello, HostInfo, LoginResult, LoginStatus, PROTOCOL_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, watch};
use tokio::task::{AbortHandle, JoinHandle};
use zeroize::Zeroizing;

const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// The client may be showing a fingerprint prompt before it logs in.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const MAX_ATTEMPTS_PER_CONNECTION: u32 = 5;
/// How long the person at the host has to allow a connection.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// A connection waiting for the host user's permission.
#[derive(Clone, Debug)]
pub struct ApprovalRequest {
    pub peer: SocketAddr,
    pub fingerprint: Fingerprint,
    pub device_name: String,
    pub username: String,
}

/// Future answering an [`ApprovalRequest`].
pub type Approval = Pin<Box<dyn Future<Output = bool> + Send>>;

/// Asks the person at the host whether a connection may proceed.
#[derive(Clone)]
pub struct Approver(pub Arc<dyn Fn(ApprovalRequest) -> Approval + Send + Sync>);

impl std::fmt::Debug for Approver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Approver")
    }
}

#[derive(Clone, Debug)]
pub struct HostSettings {
    pub device_name: String,
    pub port: u16,
    pub listen: Vec<IpAddr>,
    pub allowed_networks: AllowList,
    pub credentials: Credentials,
    pub discoverable: bool,
    /// Service names advertised in `HostInfo`.
    pub services: Vec<String>,
    /// When set, every login also needs the host user's permission.
    pub approver: Option<Approver>,
}

#[derive(Clone, Debug)]
pub enum HostEvent {
    Listening(Vec<SocketAddr>),
    Refused { peer: SocketAddr },
    LoginFailed { peer: SocketAddr, status: LoginStatus },
    SessionOpened(SessionInfo),
    SessionClosed { id: u64, reason: String },
}

#[derive(Clone, Debug)]
pub struct SessionInfo {
    pub id: u64,
    pub peer: SocketAddr,
    pub fingerprint: Fingerprint,
    pub device_name: String,
    pub username: String,
}

/// An authenticated connection handed to the [`SessionHandler`].
pub struct HostSession {
    pub info: SessionInfo,
    pub sender: Sender,
    pub receiver: Receiver,
    /// Becomes `true` when the host shuts down.
    pub shutdown: watch::Receiver<bool>,
}

pub type BoxFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send>>;

/// Runs the services of one session until it ends; the returned error text
/// is reported in [`HostEvent::SessionClosed`].
pub trait SessionHandler: Send + Sync + 'static {
    /// Displays announced in `HostInfo`.
    fn displays(&self) -> Vec<Display> {
        Vec::new()
    }

    fn run(&self, session: HostSession) -> BoxFuture;
}

/// Answers pings and waits for `Close`: enough for connectivity checks.
pub struct ControlOnly;

impl SessionHandler for ControlOnly {
    fn run(&self, session: HostSession) -> BoxFuture {
        Box::pin(control_loop(session))
    }
}

async fn control_loop(mut session: HostSession) -> Result<(), String> {
    loop {
        tokio::select! {
            msg = session.receiver.recv() => match msg {
                Ok(Msg::Ping(p)) => session.sender.send(Msg::Pong(p)).await.map_err(|e| e.to_string())?,
                Ok(Msg::Close(c)) => {
                    tracing::debug!("client closed: {}", c.reason);
                    return Ok(());
                }
                Ok(other) => tracing::debug!("ignoring {other:?}"),
                Err(okno_net::Error::Closed) => return Ok(()),
                Err(e) => return Err(e.to_string()),
            },
            _ = session.shutdown.changed() => return Ok(()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum HostError {
    #[error("cannot listen on {addr}: {source}")]
    Bind { addr: SocketAddr, source: std::io::Error },
}

struct Shared {
    identity: Identity,
    settings: HostSettings,
    /// Replaceable while running (`Host::set_credentials`).
    credentials: RwLock<Credentials>,
    approver: RwLock<Option<Approver>>,
    handler: Arc<dyn SessionHandler>,
    throttle: LoginThrottle,
    events: broadcast::Sender<HostEvent>,
    shutdown: watch::Receiver<bool>,
    next_id: AtomicU64,
    /// Connection tasks by session id, for `Host::disconnect`.
    connections: Mutex<HashMap<u64, AbortHandle>>,
}

/// A running host. Dropping it stops listening and ends all sessions.
pub struct Host {
    shared: Arc<Shared>,
    addrs: Vec<SocketAddr>,
    stop: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
    announcer: Mutex<Option<Announcer>>,
}

impl Host {
    pub async fn start(
        identity: Identity,
        settings: HostSettings,
        handler: Arc<dyn SessionHandler>,
    ) -> Result<Self, HostError> {
        let binds: Vec<SocketAddr> = if settings.listen.is_empty() {
            vec![SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), settings.port)]
        } else {
            settings.listen.iter().map(|ip| SocketAddr::new(*ip, settings.port)).collect()
        };
        let mut listeners = Vec::new();
        for addr in binds {
            listeners.push(bind(addr).await.map_err(|source| HostError::Bind { addr, source })?);
        }
        let addrs: Vec<SocketAddr> = listeners.iter().filter_map(|l| l.local_addr().ok()).collect();

        let announcement = HostAnnouncement {
            name: settings.device_name.clone(),
            os: crate::os_name().into(),
            fingerprint: identity.fingerprint(),
            port: addrs[0].port(),
        };
        let announcer = if settings.discoverable { Some(Announcer::start(announcement).await) } else { None };

        let (events, _) = broadcast::channel(64);
        let (stop, shutdown) = watch::channel(false);
        let shared = Arc::new(Shared {
            identity,
            credentials: RwLock::new(settings.credentials.clone()),
            approver: RwLock::new(settings.approver.clone()),
            settings,
            handler,
            throttle: LoginThrottle::default(),
            events: events.clone(),
            shutdown,
            next_id: AtomicU64::new(1),
            connections: Mutex::default(),
        });
        let tasks = listeners.into_iter().map(|listener| tokio::spawn(accept_loop(listener, shared.clone()))).collect();
        let _ = events.send(HostEvent::Listening(addrs.clone()));
        Ok(Self { shared, addrs, stop, tasks, announcer: Mutex::new(announcer) })
    }

    pub fn local_addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.shared.events.subscribe()
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.shared.identity.fingerprint()
    }

    /// Turns asking the host user on (`Some`) or off for new connections.
    pub fn set_approver(&self, approver: Option<Approver>) {
        *self.shared.approver.write().unwrap() = approver;
    }

    /// New logins are checked against `credentials`; open sessions stay.
    pub fn set_credentials(&self, credentials: Credentials) {
        *self.shared.credentials.write().unwrap() = credentials;
    }

    /// Starts or stops announcing the host on the network.
    pub async fn set_discoverable(&self, on: bool, name: &str) {
        let current = self.announcer.lock().unwrap().take();
        drop(current);
        if on {
            let announcer = Announcer::start(HostAnnouncement {
                name: name.to_owned(),
                os: crate::os_name().into(),
                fingerprint: self.fingerprint(),
                port: self.addrs[0].port(),
            })
            .await;
            *self.announcer.lock().unwrap() = Some(announcer);
        }
    }

    /// Ends a session; the client sees the connection close.
    pub fn disconnect(&self, id: u64) -> bool {
        match self.shared.connections.lock().unwrap().remove(&id) {
            Some(handle) => {
                handle.abort();
                let _ = self.shared.events.send(HostEvent::SessionClosed { id, reason: "disconnected by host".into() });
                true
            }
            None => false,
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.stop.send(true);
        for task in &self.tasks {
            task.abort();
        }
        for (_, handle) in self.shared.connections.lock().unwrap().drain() {
            handle.abort();
        }
    }
}

/// Binds dual-stack when listening on `::` (Windows defaults to IPv6-only),
/// falling back to IPv4 on systems without IPv6.
async fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    match bind_socket(addr) {
        Err(e) if addr.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) => {
            tracing::debug!("IPv6 listen failed ({e}), using IPv4");
            bind_socket(SocketAddr::new(std::net::Ipv4Addr::UNSPECIFIED.into(), addr.port()))
        }
        other => other,
    }
}

fn bind_socket(addr: SocketAddr) -> std::io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.ip() == IpAddr::V6(Ipv6Addr::UNSPECIFIED) {
        socket.set_only_v6(false)?;
    }
    // Lets a restarted host rebind while old connections sit in TIME_WAIT.
    // (On Windows this flag would allow stealing the port, so skip it there.)
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(128)?;
    TcpListener::from_std(socket.into())
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        let (stream, peer) = match listener.accept().await {
            // Dual-stack sockets report IPv4 peers as ::ffff:a.b.c.d.
            Ok((stream, peer)) => (stream, SocketAddr::new(peer.ip().to_canonical(), peer.port())),
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if !shared.settings.allowed_networks.allows(peer.ip()) {
            tracing::info!("refused {peer}: not in allowed networks");
            let _ = shared.events.send(HostEvent::Refused { peer });
            continue;
        }
        let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
        let task_shared = shared.clone();
        // Hold the lock across spawn so the task cannot look itself up
        // before it is registered.
        let mut connections = shared.connections.lock().unwrap();
        let task = tokio::spawn(async move {
            let shared = task_shared;
            let result = serve(stream, peer, id, &shared).await;
            // Gone from the map means `disconnect` already reported it.
            let known = shared.connections.lock().unwrap().remove(&id).is_some();
            let reason = match result {
                Ok(Some(reason)) => reason,
                Ok(None) => return,
                Err(e) => e,
            };
            if known {
                let _ = shared.events.send(HostEvent::SessionClosed { id, reason });
            }
        });
        connections.insert(id, task.abort_handle());
        drop(connections);
    }
}

/// Returns `Ok(None)` when the connection never became a session.
async fn serve(stream: TcpStream, peer: SocketAddr, id: u64, shared: &Shared) -> Result<Option<String>, String> {
    let conn = match okno_net::accept(stream, &shared.identity).await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!("handshake with {peer} failed: {e}");
            return Ok(None);
        }
    };
    let fingerprint = conn.remote_fingerprint();
    let (sender, mut receiver) = conn.into_parts();

    let hello = match tokio::time::timeout(HELLO_TIMEOUT, receiver.recv()).await {
        Ok(Ok(Msg::Hello(h))) => h,
        _ => return Ok(None),
    };
    if hello.version != PROTOCOL_VERSION {
        let _ = sender
            .send(Msg::Error(ErrorMsg { message: format!("unsupported protocol version {}", hello.version) }))
            .await;
        return Ok(None);
    }
    sender
        .send(Msg::Hello(Hello {
            version: PROTOCOL_VERSION,
            device_name: shared.settings.device_name.clone(),
            os: crate::os_name().into(),
            capabilities: shared.settings.services.clone(),
        }))
        .await
        .map_err(|e| e.to_string())?;

    let Some(username) = login(&sender, &mut receiver, peer, (fingerprint, &hello.device_name), shared).await? else {
        return Ok(None);
    };

    sender
        .send(Msg::HostInfo(HostInfo {
            displays: shared.handler.displays(),
            mac_addresses: local_mac_addresses(),
            services: shared.settings.services.clone(),
        }))
        .await
        .map_err(|e| e.to_string())?;

    let info = SessionInfo { id, peer, fingerprint, device_name: hello.device_name, username };
    tracing::info!("session {id} opened from {peer} ({})", info.device_name);
    let _ = shared.events.send(HostEvent::SessionOpened(info.clone()));
    let session = HostSession { info, sender, receiver, shutdown: shared.shutdown.clone() };
    Ok(Some(match shared.handler.run(session).await {
        Ok(()) => "closed".into(),
        Err(e) => e,
    }))
}

/// Runs login attempts; returns the username on success, `None` when the
/// client gave up or used up its attempts.
async fn login(
    sender: &Sender,
    receiver: &mut Receiver,
    peer: SocketAddr,
    (fingerprint, device_name): (Fingerprint, &str),
    shared: &Shared,
) -> Result<Option<String>, String> {
    let reply = |status: LoginStatus, retry: Duration| {
        Msg::LoginResult(LoginResult { status: status as i32, retry_after_ms: retry.as_millis() as u64 })
    };
    for _ in 0..MAX_ATTEMPTS_PER_CONNECTION {
        let login = match tokio::time::timeout(LOGIN_TIMEOUT, receiver.recv()).await {
            Ok(Ok(Msg::Login(l))) => l,
            Ok(Ok(Msg::Close(_))) | Ok(Err(_)) | Err(_) => return Ok(None),
            Ok(Ok(_)) => return Err("unexpected message before login".into()),
        };
        let password = Zeroizing::new(login.password);

        let status = if let ThrottleDecision::RetryAfter(wait) = shared.throttle.check(peer.ip(), Instant::now()) {
            sender.send(reply(LoginStatus::Throttled, wait)).await.map_err(|e| e.to_string())?;
            LoginStatus::Throttled
        } else if let Some(_permit) = shared.throttle.try_begin() {
            let creds = shared.credentials.read().unwrap().clone();
            let username = login.username.clone();
            let ok = tokio::task::spawn_blocking(move || creds.verify(&username, password)).await.unwrap_or(false);
            if ok {
                shared.throttle.record_success(peer.ip());
                let approver = shared.approver.read().unwrap().clone();
                if let Some(approver) = approver {
                    let request = ApprovalRequest {
                        peer,
                        fingerprint,
                        device_name: device_name.to_owned(),
                        username: login.username.clone(),
                    };
                    let allowed = tokio::time::timeout(APPROVAL_TIMEOUT, (approver.0)(request)).await.unwrap_or(false);
                    if !allowed {
                        tracing::info!("connection from {peer} declined by the host user");
                        let _ = sender.send(reply(LoginStatus::Denied, Duration::ZERO)).await;
                        let _ = shared.events.send(HostEvent::LoginFailed { peer, status: LoginStatus::Denied });
                        return Ok(None);
                    }
                }
                sender.send(reply(LoginStatus::Ok, Duration::ZERO)).await.map_err(|e| e.to_string())?;
                return Ok(Some(login.username));
            }
            let wait = shared.throttle.record_failure(peer.ip(), Instant::now());
            sender.send(reply(LoginStatus::BadCredentials, wait)).await.map_err(|e| e.to_string())?;
            LoginStatus::BadCredentials
        } else {
            sender.send(reply(LoginStatus::Busy, Duration::from_secs(1))).await.map_err(|e| e.to_string())?;
            LoginStatus::Busy
        };
        tracing::info!("login from {peer} rejected: {status:?}");
        let _ = shared.events.send(HostEvent::LoginFailed { peer, status });
    }
    let _ = sender.send(Msg::Close(Close { reason: "too many attempts".into() })).await;
    Ok(None)
}

/// MAC addresses of physical interfaces, for Wake-on-LAN.
fn local_mac_addresses() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        let mut macs = Vec::new();
        let Ok(dir) = std::fs::read_dir("/sys/class/net") else { return macs };
        for entry in dir.flatten() {
            let path = entry.path();
            // Virtual interfaces (bridges, veth, tun) have no `device` link.
            if !path.join("device").exists() {
                continue;
            }
            if let Ok(mac) = std::fs::read_to_string(path.join("address")) {
                let mac = mac.trim().to_owned();
                if mac != "00:00:00:00:00:00" && !macs.contains(&mac) {
                    macs.push(mac);
                }
            }
        }
        macs.sort();
        macs
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}
