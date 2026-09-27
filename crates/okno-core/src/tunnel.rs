//! TCP port forwarding from the client to addresses the host can reach.
//!
//! The client listens on a local port; every accepted connection becomes a
//! tunnel:
//!
//! ```text
//! C Open("host:port")  → H Opened(true) | Opened(false)
//! C/H Data             bytes in either direction
//! C/H Ack(n)           n bytes were written out; the peer may send n more
//! C/H Closed(true)     that side sends no more (half-close)
//! C/H Closed(false)    the tunnel is aborted
//! ```
//!
//! Each side keeps at most [`WINDOW`] bytes unacknowledged per tunnel, so a
//! slow local socket slows the peer down instead of filling memory.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okno_net::Sender;
use okno_proto::TunnelMsg;
use okno_proto::envelope::Msg;
use okno_proto::tunnel_msg::Op;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio::task::JoinHandle;

pub const SERVICE_TUNNEL: &str = "tunnel";

/// Unacknowledged bytes allowed per tunnel and direction.
pub const WINDOW: usize = 1024 * 1024;
const READ_CHUNK: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn msg(id: u32, op: Op) -> Msg {
    Msg::Tunnel(TunnelMsg { id, op: Some(op) })
}

enum Inbound {
    Data(Vec<u8>),
    Eof,
    Reset,
}

/// One tunnel as the dispatcher sees it.
struct Link {
    inbound: mpsc::UnboundedSender<Inbound>,
    /// Bytes handed to the pump and not yet written out.
    queued: AtomicUsize,
    /// Bytes this side may still send.
    credit: Semaphore,
}

type Links = Arc<Mutex<HashMap<u32, Arc<Link>>>>;

fn new_link() -> (Arc<Link>, mpsc::UnboundedReceiver<Inbound>) {
    let (inbound, rx) = mpsc::unbounded_channel();
    (Arc::new(Link { inbound, queued: AtomicUsize::new(0), credit: Semaphore::new(WINDOW) }), rx)
}

/// Routes Data, Ack and Closed from the peer to their tunnel.
fn deliver(links: &Links, id: u32, op: Op) {
    let mut links = links.lock().unwrap();
    let Some(link) = links.get(&id) else { return };
    match op {
        Op::Data(data) => {
            if link.queued.fetch_add(data.len(), Ordering::Relaxed) + data.len() > WINDOW {
                tracing::debug!("tunnel {id}: peer overran the window");
                let _ = link.inbound.send(Inbound::Reset);
                links.remove(&id);
                return;
            }
            let _ = link.inbound.send(Inbound::Data(data));
        }
        Op::Ack(n) => {
            // Never more credit than the window, whatever the peer claims.
            let room = WINDOW.saturating_sub(link.credit.available_permits());
            link.credit.add_permits((n as usize).min(room));
        }
        Op::Closed(true) => {
            let _ = link.inbound.send(Inbound::Eof);
        }
        Op::Closed(false) => {
            let _ = link.inbound.send(Inbound::Reset);
            link.credit.close();
            links.remove(&id);
        }
        Op::Open(_) | Op::Opened(_) => {}
    }
}

/// Pumps a TCP stream to and from tunnel messages until both directions
/// have finished, or either fails.
async fn pump(stream: TcpStream, id: u32, sender: &Sender, link: &Link, mut inbound: mpsc::UnboundedReceiver<Inbound>) {
    let (mut rd, mut wr) = stream.into_split();
    // Each returns whether its direction finished cleanly.
    let outbound = async {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            let n = match rd.read(&mut buf).await {
                Ok(0) => return true,
                Ok(n) => n,
                Err(_) => return false,
            };
            match link.credit.acquire_many(n as u32).await {
                Ok(permit) => permit.forget(),
                Err(_) => return false,
            }
            if sender.send(msg(id, Op::Data(buf[..n].to_vec()))).await.is_err() {
                return false;
            }
        }
    };
    let incoming = async {
        while let Some(item) = inbound.recv().await {
            match item {
                Inbound::Data(data) => {
                    if wr.write_all(&data).await.is_err() {
                        return false;
                    }
                    link.queued.fetch_sub(data.len(), Ordering::Relaxed);
                    let _ = sender.send(msg(id, Op::Ack(data.len() as u32))).await;
                }
                Inbound::Eof => {
                    let _ = wr.shutdown().await;
                    return true;
                }
                Inbound::Reset => return false,
            }
        }
        false
    };
    tokio::pin!(outbound, incoming);
    let (mut sent_all, mut got_all) = (false, false);
    let clean = loop {
        tokio::select! {
            ok = &mut outbound, if !sent_all => {
                if !ok {
                    break false;
                }
                sent_all = true;
                let _ = sender.send(msg(id, Op::Closed(true))).await;
            }
            ok = &mut incoming, if !got_all => {
                if !ok {
                    break false;
                }
                got_all = true;
            }
        }
        if sent_all && got_all {
            break true;
        }
    };
    if !clean {
        let _ = sender.send(msg(id, Op::Closed(false))).await;
    }
}

// ---- Host ------------------------------------------------------------------

/// Tunnels of one host session.
pub struct TunnelService {
    sender: Sender,
    links: Links,
    tasks: HashMap<u32, JoinHandle<()>>,
}

impl TunnelService {
    pub fn new(sender: Sender) -> Self {
        Self { sender, links: Arc::default(), tasks: HashMap::new() }
    }

    pub async fn handle(&mut self, request: TunnelMsg) {
        let id = request.id;
        match request.op {
            Some(Op::Open(target)) => {
                self.tasks.retain(|_, t| !t.is_finished());
                if self.tasks.contains_key(&id) {
                    tracing::debug!("tunnel {id}: id already in use");
                    return;
                }
                let (link, rx) = new_link();
                self.links.lock().unwrap().insert(id, link.clone());
                let sender = self.sender.clone();
                let links = self.links.clone();
                let task = tokio::spawn(async move {
                    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await {
                        Ok(Ok(stream)) => {
                            let _ = stream.set_nodelay(true);
                            let _ = sender.send(msg(id, Op::Opened(true))).await;
                            pump(stream, id, &sender, &link, rx).await;
                        }
                        _ => {
                            tracing::debug!("tunnel {id}: cannot reach {target}");
                            let _ = sender.send(msg(id, Op::Opened(false))).await;
                        }
                    }
                    links.lock().unwrap().remove(&id);
                });
                self.tasks.insert(id, task);
            }
            Some(op) => deliver(&self.links, id, op),
            None => {}
        }
    }
}

impl Drop for TunnelService {
    fn drop(&mut self) {
        for (_, task) in self.tasks.drain() {
            task.abort();
        }
    }
}

// ---- Client ----------------------------------------------------------------

/// Client side of port forwarding; cheap to clone.
#[derive(Clone)]
pub struct Tunnels {
    sender: Sender,
    links: Links,
    /// Replies to Open, by tunnel id.
    opened: Arc<Mutex<HashMap<u32, tokio::sync::oneshot::Sender<bool>>>>,
    next: Arc<AtomicU32>,
}

/// A running forward; dropping it stops listening and closes its tunnels.
pub struct Forward {
    local: SocketAddr,
    target: String,
    task: JoinHandle<()>,
}

impl Forward {
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    pub fn target(&self) -> &str {
        &self.target
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Tunnels {
    pub(crate) fn new(sender: Sender) -> Self {
        Self { sender, links: Arc::default(), opened: Arc::default(), next: Arc::new(AtomicU32::new(1)) }
    }

    pub(crate) fn dispatch(&self, reply: TunnelMsg) {
        let id = reply.id;
        match reply.op {
            Some(Op::Opened(ok)) => {
                if let Some(waiter) = self.opened.lock().unwrap().remove(&id) {
                    let _ = waiter.send(ok);
                }
            }
            Some(op) => deliver(&self.links, id, op),
            None => {}
        }
    }

    /// Listens on `local` (use port 0 for any free port) and forwards each
    /// connection to `target` (`host:port` as seen from the host).
    pub async fn forward(&self, local: SocketAddr, target: String) -> std::io::Result<Forward> {
        let listener = TcpListener::bind(local).await?;
        let local = listener.local_addr()?;
        let tunnels = self.clone();
        let shown = target.clone();
        let task = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let _ = stream.set_nodelay(true);
                let tunnels = tunnels.clone();
                let target = target.clone();
                connections.spawn(async move { tunnels.connect(stream, target).await });
            }
        });
        Ok(Forward { local, target: shown, task })
    }

    async fn connect(&self, stream: TcpStream, target: String) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (link, rx) = new_link();
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
        self.links.lock().unwrap().insert(id, link.clone());
        self.opened.lock().unwrap().insert(id, opened_tx);
        if self.sender.send(msg(id, Op::Open(target))).await.is_ok() {
            let ok =
                tokio::time::timeout(CONNECT_TIMEOUT * 2, opened_rx).await.ok().and_then(Result::ok).unwrap_or(false);
            if ok {
                pump(stream, id, &self.sender, &link, rx).await;
            }
        }
        self.links.lock().unwrap().remove(&id);
        self.opened.lock().unwrap().remove(&id);
    }
}
