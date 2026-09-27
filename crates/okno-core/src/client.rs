//! The controlling side.
//!
//! ```text
//! let mut pending = client::open(&endpoint, &identity, &trust, name).await?;
//! match pending.trust { New | Changed { .. } => ask the user, _ => {} }
//! pending.login(user, password).await?;       // may be retried
//! let session = pending.into_session();
//! ```

use std::time::{Duration, Instant};

use okno_auth::{TrustDecision, TrustStore};
use okno_net::{Connection, Fingerprint, Identity, Receiver, Sender};
use okno_proto::envelope::Msg;
use okno_proto::{Close, Hello, HostInfo, Login, LoginStatus, PROTOCOL_VERSION, Ping};
use tokio::net::TcpStream;

use crate::Endpoint;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REPLY_TIMEOUT: Duration = Duration::from_secs(15);
/// The host user may take a while to allow the connection.
const LOGIN_REPLY_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot resolve {0}")]
    Resolve(String),
    #[error("cannot connect to {endpoint}: {reason}")]
    Connect { endpoint: String, reason: String },
    #[error(transparent)]
    Net(#[from] okno_net::Error),
    #[error("the host did not answer in time")]
    Timeout,
    #[error("the host uses an incompatible protocol version ({0})")]
    Version(u32),
    #[error("the host refused: {0}")]
    Refused(String),
    #[error("wrong username or password")]
    BadCredentials { retry_after: Duration },
    #[error("too many failed attempts; try again in {} s", retry_after.as_secs().max(1))]
    Throttled { retry_after: Duration },
    #[error("the host is busy; try again")]
    Busy,
    #[error("the host has no password set")]
    NotConfigured,
    #[error("the person at the host declined the connection")]
    Denied,
    #[error("unexpected reply from the host")]
    Protocol,
}

/// A connection whose key is known but which is not logged in yet.
pub struct Pending {
    pub endpoint: Endpoint,
    pub host: Hello,
    pub fingerprint: Fingerprint,
    pub trust: TrustDecision,
    conn: Connection,
    host_info: Option<HostInfo>,
    /// A reply timed out part-way through a record, so the stream can no
    /// longer be read; the connection is closed.
    broken: bool,
}

/// A logged-in session.
pub struct Session {
    pub endpoint: Endpoint,
    pub host: Hello,
    pub host_info: HostInfo,
    pub fingerprint: Fingerprint,
    pub sender: Sender,
    pub receiver: Receiver,
}

/// Connects, runs the handshake and exchanges `Hello`.
pub async fn open(
    endpoint: &Endpoint,
    identity: &Identity,
    trust: &TrustStore,
    device_name: &str,
) -> Result<Pending, ClientError> {
    let stream = dial(endpoint).await?;
    let mut conn = okno_net::connect(stream, identity).await?;
    conn.sender()
        .send(Msg::Hello(Hello {
            version: PROTOCOL_VERSION,
            device_name: device_name.to_owned(),
            os: crate::os_name().into(),
            capabilities: Vec::new(),
        }))
        .await?;
    let host = match recv(conn.receiver()).await? {
        Msg::Hello(h) if h.version == PROTOCOL_VERSION => h,
        Msg::Hello(h) => return Err(ClientError::Version(h.version)),
        Msg::Error(e) => return Err(ClientError::Refused(e.message)),
        _ => return Err(ClientError::Protocol),
    };
    let fingerprint = conn.remote_fingerprint();
    Ok(Pending {
        trust: trust.check(&endpoint.to_string(), &fingerprint),
        endpoint: endpoint.clone(),
        host,
        fingerprint,
        conn,
        host_info: None,
        broken: false,
    })
}

/// Tries each resolved address in turn.
async fn dial(endpoint: &Endpoint) -> Result<TcpStream, ClientError> {
    let addrs: Vec<_> = tokio::net::lookup_host((endpoint.host.as_str(), endpoint.port))
        .await
        .map_err(|_| ClientError::Resolve(endpoint.host.clone()))?
        .collect();
    if addrs.is_empty() {
        return Err(ClientError::Resolve(endpoint.host.clone()));
    }
    let mut last = String::new();
    for addr in addrs {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "timed out".into(),
        }
    }
    Err(ClientError::Connect { endpoint: endpoint.to_string(), reason: last })
}

async fn recv(receiver: &mut Receiver) -> Result<Msg, ClientError> {
    recv_within(receiver, REPLY_TIMEOUT).await
}

async fn recv_within(receiver: &mut Receiver, timeout: Duration) -> Result<Msg, ClientError> {
    match tokio::time::timeout(timeout, receiver.recv()).await {
        Ok(Ok(Msg::Close(c))) => Err(ClientError::Refused(c.reason)),
        Ok(msg) => Ok(msg?),
        Err(_) => Err(ClientError::Timeout),
    }
}

impl Pending {
    /// Sends credentials. On `BadCredentials`, `Throttled` or `Busy` the same
    /// connection can be used for another attempt.
    pub async fn login(&mut self, username: &str, password: &str) -> Result<(), ClientError> {
        if self.broken {
            return Err(ClientError::Timeout);
        }
        self.conn
            .sender()
            .send(Msg::Login(Login { username: username.to_owned(), password: password.to_owned() }))
            .await?;
        let result = match self.reply(LOGIN_REPLY_TIMEOUT).await? {
            Msg::LoginResult(r) => r,
            _ => return Err(ClientError::Protocol),
        };
        let retry_after = Duration::from_millis(result.retry_after_ms);
        match LoginStatus::try_from(result.status).unwrap_or(LoginStatus::Unspecified) {
            LoginStatus::Ok => {}
            LoginStatus::BadCredentials => return Err(ClientError::BadCredentials { retry_after }),
            LoginStatus::Throttled => return Err(ClientError::Throttled { retry_after }),
            LoginStatus::Busy => return Err(ClientError::Busy),
            LoginStatus::NotConfigured => return Err(ClientError::NotConfigured),
            LoginStatus::Denied => return Err(ClientError::Denied),
            LoginStatus::Unspecified => return Err(ClientError::Protocol),
        }
        match self.reply(REPLY_TIMEOUT).await? {
            Msg::HostInfo(info) => self.host_info = Some(info),
            _ => return Err(ClientError::Protocol),
        }
        Ok(())
    }

    async fn reply(&mut self, timeout: Duration) -> Result<Msg, ClientError> {
        let reply = recv_within(self.conn.receiver(), timeout).await;
        if matches!(reply, Err(ClientError::Timeout)) {
            self.broken = true;
            self.conn.sender().close();
        }
        reply
    }

    pub fn is_logged_in(&self) -> bool {
        self.host_info.is_some()
    }

    /// # Panics
    /// If [`login`](Self::login) has not succeeded.
    pub fn into_session(self) -> Session {
        let (sender, receiver) = self.conn.into_parts();
        Session {
            endpoint: self.endpoint,
            host: self.host,
            host_info: self.host_info.expect("login succeeded"),
            fingerprint: self.fingerprint,
            sender,
            receiver,
        }
    }
}

impl Session {
    /// Round-trip time of one ping. Only valid while nothing else reads the
    /// receiver. After a timeout the session is unusable (a record may have
    /// been cut in half) and is closed.
    pub async fn ping(&mut self) -> Result<Duration, ClientError> {
        let nonce = rand::random();
        let start = Instant::now();
        self.sender.send(Msg::Ping(Ping { nonce })).await?;
        loop {
            let reply = recv(&mut self.receiver).await;
            if matches!(reply, Err(ClientError::Timeout)) {
                self.sender.close();
            }
            if let Msg::Pong(p) = reply?
                && p.nonce == nonce
            {
                return Ok(start.elapsed());
            }
        }
    }

    pub async fn close(self, reason: &str) {
        let _ = self.sender.send(Msg::Close(Close { reason: reason.to_owned() })).await;
    }
}
