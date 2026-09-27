//! TCP port forwarding from the client to addresses the host can reach.
//!
//! The client listens on a local port; every accepted connection becomes a
//! tunnel:
//!
//! ```text
//! C Open("host:port")  → H Opened(true) | Opened(false)
//! C/H Data             bytes in either direction
//! C/H Closed           that side finished; the tunnel ends
//! ```

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okno_net::Sender;
use okno_proto::TunnelMsg;
use okno_proto::envelope::Msg;
use okno_proto::tunnel_msg::Op;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub const SERVICE_TUNNEL: &str = "tunnel";

const READ_CHUNK: usize = 32 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

fn msg(id: u32, op: Op) -> Msg {
    Msg::Tunnel(TunnelMsg { id, op: Some(op) })
}

/// Pumps a TCP stream to and from tunnel messages until either side closes.
/// Data from the peer arrives on `incoming`; `None` there means Closed.
async fn pump(stream: TcpStream, id: u32, sender: Sender, mut incoming: mpsc::UnboundedReceiver<Option<Vec<u8>>>) {
    let (mut rd, mut wr) = stream.into_split();
    let out_sender = sender.clone();
    let outbound = async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out_sender.send(msg(id, Op::Data(buf[..n].to_vec()))).await.is_err() {
                        break;
                    }
                }
            }
        }
    };
    let inbound = async move {
        while let Some(Some(data)) = incoming.recv().await {
            if wr.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    };
    // Either direction ending ends the tunnel.
    tokio::select! {
        _ = outbound => {}
        _ = inbound => {}
    }
    let _ = sender.send(msg(id, Op::Closed(true))).await;
}

type Routes = Arc<Mutex<HashMap<u32, mpsc::UnboundedSender<Option<Vec<u8>>>>>>;

// ---- Host ------------------------------------------------------------------

/// Tunnels of one host session.
pub struct TunnelService {
    sender: Sender,
    routes: Routes,
    tasks: HashMap<u32, JoinHandle<()>>,
}

impl TunnelService {
    pub fn new(sender: Sender) -> Self {
        Self { sender, routes: Arc::default(), tasks: HashMap::new() }
    }

    pub async fn handle(&mut self, request: TunnelMsg) {
        let id = request.id;
        match request.op {
            Some(Op::Open(target)) => {
                let (tx, rx) = mpsc::unbounded_channel();
                self.routes.lock().unwrap().insert(id, tx);
                let sender = self.sender.clone();
                let routes = self.routes.clone();
                let task = tokio::spawn(async move {
                    match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target)).await {
                        Ok(Ok(stream)) => {
                            let _ = stream.set_nodelay(true);
                            let _ = sender.send(msg(id, Op::Opened(true))).await;
                            pump(stream, id, sender, rx).await;
                        }
                        _ => {
                            tracing::debug!("tunnel {id}: cannot reach {target}");
                            let _ = sender.send(msg(id, Op::Opened(false))).await;
                        }
                    }
                    routes.lock().unwrap().remove(&id);
                });
                self.tasks.retain(|_, t| !t.is_finished());
                self.tasks.insert(id, task);
            }
            Some(Op::Data(data)) => {
                if let Some(route) = self.routes.lock().unwrap().get(&id) {
                    let _ = route.send(Some(data));
                }
            }
            Some(Op::Closed(_)) => {
                if let Some(route) = self.routes.lock().unwrap().remove(&id) {
                    let _ = route.send(None);
                }
            }
            _ => {}
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
    routes: Routes,
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
        Self { sender, routes: Arc::default(), opened: Arc::default(), next: Arc::new(AtomicU32::new(1)) }
    }

    pub(crate) fn dispatch(&self, reply: TunnelMsg) {
        let id = reply.id;
        match reply.op {
            Some(Op::Opened(ok)) => {
                if let Some(waiter) = self.opened.lock().unwrap().remove(&id) {
                    let _ = waiter.send(ok);
                }
            }
            Some(Op::Data(data)) => {
                if let Some(route) = self.routes.lock().unwrap().get(&id) {
                    let _ = route.send(Some(data));
                }
            }
            Some(Op::Closed(_)) => {
                if let Some(route) = self.routes.lock().unwrap().remove(&id) {
                    let _ = route.send(None);
                }
            }
            _ => {}
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
        let (tx, rx) = mpsc::unbounded_channel();
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
        self.routes.lock().unwrap().insert(id, tx);
        self.opened.lock().unwrap().insert(id, opened_tx);
        if self.sender.send(msg(id, Op::Open(target))).await.is_err() {
            return;
        }
        let ok = tokio::time::timeout(CONNECT_TIMEOUT * 2, opened_rx).await.ok().and_then(Result::ok).unwrap_or(false);
        if ok {
            pump(stream, id, self.sender.clone(), rx).await;
        }
        self.routes.lock().unwrap().remove(&id);
        self.opened.lock().unwrap().remove(&id);
    }
}
