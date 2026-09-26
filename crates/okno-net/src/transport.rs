use std::net::SocketAddr;
use std::sync::Arc;

use okno_proto::{Channel, Envelope, MAX_MESSAGE_LEN, envelope::Msg};
use snow::StatelessTransportState;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;

use crate::{Error, FLAG_FIN, Fingerprint, MAX_FRAGMENT, MAX_RECORD, Result, read_record, write_record};

/// Queue depth per channel, in whole messages. Video is kept short so a slow
/// link drops frames at the sender instead of building latency.
const QUEUE_DEPTH: [usize; Channel::COUNT] = [64, 256, 32, 3, 16];

/// An established, authenticated-by-key connection.
pub struct Connection {
    sender: Sender,
    receiver: Receiver,
    remote_key: [u8; 32],
    peer_addr: Option<SocketAddr>,
}

impl Connection {
    pub(crate) fn new(stream: TcpStream, transport: StatelessTransportState, remote_key: [u8; 32]) -> Self {
        let peer_addr = stream.peer_addr().ok();
        let transport = Arc::new(transport);
        let (rd, wr) = stream.into_split();
        let mut txs = Vec::with_capacity(Channel::COUNT);
        let mut rxs = Vec::with_capacity(Channel::COUNT);
        for depth in QUEUE_DEPTH {
            let (tx, rx) = mpsc::channel(depth);
            txs.push(tx);
            rxs.push(rx);
        }
        let queues: [mpsc::Receiver<Vec<u8>>; Channel::COUNT] = rxs.try_into().unwrap();
        let writer_transport = transport.clone();
        tokio::spawn(async move {
            if let Err(e) = write_loop(BufWriter::with_capacity(MAX_RECORD * 2, wr), writer_transport, queues).await {
                tracing::debug!("writer stopped: {e}");
            }
        });
        Self {
            sender: Sender { queues: Arc::new(txs.try_into().unwrap()) },
            receiver: Receiver {
                rd: BufReader::with_capacity(MAX_RECORD * 2, rd),
                transport,
                nonce: 0,
                record: vec![0; MAX_RECORD],
                plain: vec![0; MAX_RECORD],
                partial: Default::default(),
            },
            remote_key,
            peer_addr,
        }
    }

    /// Static public key the peer proved during the handshake.
    pub fn remote_key(&self) -> &[u8; 32] {
        &self.remote_key
    }

    pub fn remote_fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.remote_key)
    }

    pub fn peer_addr(&self) -> Option<SocketAddr> {
        self.peer_addr
    }

    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    pub fn receiver(&mut self) -> &mut Receiver {
        &mut self.receiver
    }

    pub fn into_parts(self) -> (Sender, Receiver) {
        (self.sender, self.receiver)
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TrySendError {
    #[error("channel queue is full")]
    Full,
    #[error("connection closed")]
    Closed,
}

/// Sending half; cheap to clone. The connection's write side closes once
/// every clone is dropped and queued messages are flushed.
#[derive(Clone)]
pub struct Sender {
    queues: Arc<[mpsc::Sender<Vec<u8>>; Channel::COUNT]>,
}

impl Sender {
    fn route(msg: Msg) -> (usize, Vec<u8>) {
        let channel = Channel::of(&msg) as usize;
        (channel, Envelope::new(msg).encode_vec())
    }

    /// Queues a message, waiting while its channel is full.
    pub async fn send(&self, msg: Msg) -> Result<()> {
        let (channel, bytes) = Self::route(msg);
        self.queues[channel].send(bytes).await.map_err(|_| Error::Closed)
    }

    /// Queues a message or fails at once; use for video, where a stale frame
    /// is worth less than a dropped one.
    pub fn try_send(&self, msg: Msg) -> Result<(), TrySendError> {
        let (channel, bytes) = Self::route(msg);
        self.queues[channel].try_send(bytes).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => TrySendError::Full,
            mpsc::error::TrySendError::Closed(_) => TrySendError::Closed,
        })
    }

    pub fn is_closed(&self) -> bool {
        self.queues[0].is_closed()
    }
}

/// Receiving half.
pub struct Receiver {
    rd: BufReader<OwnedReadHalf>,
    transport: Arc<StatelessTransportState>,
    nonce: u64,
    record: Vec<u8>,
    plain: Vec<u8>,
    partial: [Vec<u8>; Channel::COUNT],
}

impl Receiver {
    /// Next complete message. Returns [`Error::Closed`] after the peer shut
    /// the connection down.
    pub async fn recv(&mut self) -> Result<Msg> {
        loop {
            let len = read_record(&mut self.rd, &mut self.record).await?;
            let n = self.transport.read_message(self.nonce, &self.record[..len], &mut self.plain)?;
            self.nonce += 1;
            if n < 2 {
                return Err(Error::Protocol("short record"));
            }
            let channel = Channel::from_u8(self.plain[0]).ok_or(Error::Protocol("unknown channel"))? as usize;
            let fin = self.plain[1] & FLAG_FIN != 0;
            let part = &mut self.partial[channel];
            if part.len() + (n - 2) > MAX_MESSAGE_LEN {
                return Err(Error::Protocol("message too large"));
            }
            part.extend_from_slice(&self.plain[2..n]);
            if fin {
                let data = std::mem::take(part);
                return Ok(Envelope::decode_msg(&data)?);
            }
        }
    }
}

async fn write_loop(
    mut wr: BufWriter<OwnedWriteHalf>,
    transport: Arc<StatelessTransportState>,
    mut queues: [mpsc::Receiver<Vec<u8>>; Channel::COUNT],
) -> Result<()> {
    let mut pending: [Option<(Vec<u8>, usize)>; Channel::COUNT] = Default::default();
    let mut open = [true; Channel::COUNT];
    let mut nonce = 0u64;
    let mut plain = Vec::with_capacity(MAX_RECORD);
    let mut out = vec![0u8; MAX_RECORD];

    loop {
        for (i, queue) in queues.iter_mut().enumerate() {
            if pending[i].is_none() && open[i] {
                match queue.try_recv() {
                    Ok(msg) => pending[i] = Some((msg, 0)),
                    Err(mpsc::error::TryRecvError::Empty) => {}
                    Err(mpsc::error::TryRecvError::Disconnected) => open[i] = false,
                }
            }
        }

        let Some(channel) = pending.iter().position(Option::is_some) else {
            wr.flush().await?;
            if !open.contains(&true) {
                wr.shutdown().await?;
                return Ok(());
            }
            let [q0, q1, q2, q3, q4] = &mut queues;
            let (i, msg) = tokio::select! {
                biased;
                m = q0.recv(), if open[0] => (0, m),
                m = q1.recv(), if open[1] => (1, m),
                m = q2.recv(), if open[2] => (2, m),
                m = q3.recv(), if open[3] => (3, m),
                m = q4.recv(), if open[4] => (4, m),
            };
            match msg {
                Some(msg) => pending[i] = Some((msg, 0)),
                None => open[i] = false,
            }
            continue;
        };

        let (msg, offset) = pending[channel].as_mut().unwrap();
        let end = (*offset + MAX_FRAGMENT).min(msg.len());
        let fin = end == msg.len();
        plain.clear();
        plain.push(channel as u8);
        plain.push(if fin { FLAG_FIN } else { 0 });
        plain.extend_from_slice(&msg[*offset..end]);
        let len = transport.write_message(nonce, &plain, &mut out)?;
        nonce += 1;
        write_record(&mut wr, &out[..len]).await?;
        if fin {
            pending[channel] = None;
        } else {
            *offset = end;
        }
    }
}
