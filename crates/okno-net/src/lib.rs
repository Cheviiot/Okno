//! Encrypted transport between two Okno devices.
//!
//! A connection starts with a Noise_XX handshake in which both sides prove
//! their long-term [`Identity`]. Afterwards every record is a Noise transport
//! message:
//!
//! ```text
//! wire record   = u16 BE length || ciphertext
//! plaintext     = channel u8 || flags u8 || fragment
//! ```
//!
//! An application message ([`okno_proto::Envelope`]) is split into fragments;
//! the last one carries [`FLAG_FIN`]. The sender interleaves fragments by
//! channel priority, so input and video overtake bulk transfers.

mod identity;
mod transport;

pub use identity::{Fingerprint, Identity};
pub use transport::{Connection, Receiver, Sender, TrySendError};

use std::time::Duration;

use okno_proto::NOISE_PROLOGUE;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub(crate) const NOISE_PARAMS: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
pub(crate) const MAX_RECORD: usize = 65535;
pub(crate) const TAG_LEN: usize = 16;
pub(crate) const HEADER_LEN: usize = 2;
pub(crate) const MAX_FRAGMENT: usize = MAX_RECORD - TAG_LEN - HEADER_LEN;
pub(crate) const FLAG_FIN: u8 = 1;

/// Time allowed for the whole handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("handshake failed: {0}")]
    Noise(#[from] snow::Error),
    #[error("handshake timed out")]
    Timeout,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error(transparent)]
    Decode(#[from] okno_proto::DecodeError),
    #[error("connection closed")]
    Closed,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Opens the secure channel as the connecting side.
pub async fn connect(stream: TcpStream, identity: &Identity) -> Result<Connection> {
    handshake(stream, identity, true).await
}

/// Opens the secure channel as the listening side.
pub async fn accept(stream: TcpStream, identity: &Identity) -> Result<Connection> {
    handshake(stream, identity, false).await
}

async fn handshake(mut stream: TcpStream, identity: &Identity, initiator: bool) -> Result<Connection> {
    stream.set_nodelay(true)?;
    let run = async {
        let builder = snow::Builder::new(NOISE_PARAMS.parse()?)
            .prologue(NOISE_PROLOGUE)?
            .local_private_key(identity.private_key())?;
        let mut state = if initiator { builder.build_initiator()? } else { builder.build_responder()? };
        let mut buf = vec![0u8; MAX_RECORD];
        let mut payload = vec![0u8; MAX_RECORD];
        // XX: -> e; <- e, ee, s, es; -> s, se
        let mut our_turn = initiator;
        while !state.is_handshake_finished() {
            if our_turn {
                let len = state.write_message(&[], &mut buf)?;
                write_record(&mut stream, &buf[..len]).await?;
            } else {
                let len = read_record(&mut stream, &mut buf).await?;
                state.read_message(&buf[..len], &mut payload)?;
            }
            our_turn = !our_turn;
        }
        let remote = state.get_remote_static().ok_or(Error::Protocol("peer sent no static key"))?;
        let remote: [u8; 32] = remote.try_into().map_err(|_| Error::Protocol("bad static key length"))?;
        let transport = state.into_stateless_transport_mode()?;
        Ok::<_, Error>((transport, remote))
    };
    let (transport, remote) = tokio::time::timeout(HANDSHAKE_TIMEOUT, run).await.map_err(|_| Error::Timeout)??;
    Ok(Connection::new(stream, transport, remote))
}

pub(crate) async fn write_record<W: AsyncWriteExt + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    debug_assert!(data.len() <= MAX_RECORD);
    w.write_all(&(data.len() as u16).to_be_bytes()).await?;
    w.write_all(data).await?;
    Ok(())
}

/// Reads one record into `buf`, returning its length. EOF before a record
/// starts is reported as [`Error::Closed`].
pub(crate) async fn read_record<R: AsyncReadExt + Unpin>(r: &mut R, buf: &mut [u8]) -> Result<usize> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Closed),
        Err(e) => return Err(e.into()),
    }
    let len = u16::from_be_bytes(len) as usize;
    r.read_exact(&mut buf[..len]).await?;
    Ok(len)
}
