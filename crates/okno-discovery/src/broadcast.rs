//! UDP broadcast discovery on [`okno_proto::DISCOVERY_PORT`].
//!
//! ```text
//! query    = "OKNO" 0x01
//! announce = "OKNO" 0x02 || protobuf Announcement
//! ```
//! A client broadcasts a query; every host replies to the sender with an
//! announcement. The reply's source IP is the address to connect to.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use okno_net::Fingerprint;
use okno_proto::DISCOVERY_PORT;
use prost::Message;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::{Error, HostAnnouncement, Peer, Source, acceptable};

const MAGIC: &[u8; 4] = b"OKNO";
const QUERY: u8 = 1;
const ANNOUNCE: u8 = 2;

#[derive(Clone, PartialEq, ::prost::Message)]
struct Announcement {
    #[prost(uint32, tag = "1")]
    version: u32,
    #[prost(string, tag = "2")]
    name: String,
    #[prost(string, tag = "3")]
    os: String,
    #[prost(bytes = "vec", tag = "4")]
    fingerprint: Vec<u8>,
    #[prost(uint32, tag = "5")]
    port: u32,
}

fn encode_announce(info: &HostAnnouncement) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    out.push(ANNOUNCE);
    Announcement {
        version: okno_proto::PROTOCOL_VERSION,
        name: info.name.clone(),
        os: info.os.clone(),
        fingerprint: info.fingerprint.0.to_vec(),
        port: info.port.into(),
    }
    .encode(&mut out)
    .expect("Vec grows");
    out
}

fn decode_announce(packet: &[u8], from: IpAddr) -> Option<Peer> {
    let body = packet.strip_prefix(MAGIC)?.strip_prefix(&[ANNOUNCE])?;
    let a = Announcement::decode(body).ok()?;
    let fingerprint = Fingerprint(a.fingerprint.as_slice().try_into().ok()?);
    let port = u16::try_from(a.port).ok().filter(|p| *p != 0)?;
    acceptable(a.version, &a.name).then(|| Peer {
        name: a.name,
        os: a.os,
        fingerprint,
        addresses: vec![SocketAddr::new(from, port)],
        sources: vec![Source::Broadcast],
    })
}

fn is_query(packet: &[u8]) -> bool {
    packet.len() == 5 && packet.starts_with(MAGIC) && packet[4] == QUERY
}

/// Answers broadcast queries until dropped.
pub struct BroadcastResponder {
    task: JoinHandle<()>,
}

impl BroadcastResponder {
    pub async fn start(info: HostAnnouncement) -> Result<Self, Error> {
        let socket = shared_socket(SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)))?;
        let reply = encode_announce(&info);
        let task = tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                match socket.recv_from(&mut buf).await {
                    Ok((n, from)) if is_query(&buf[..n]) => {
                        if let Err(e) = socket.send_to(&reply, from).await {
                            tracing::debug!("discovery reply to {from} failed: {e}");
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("discovery socket error: {e}");
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
        });
        Ok(Self { task })
    }
}

impl Drop for BroadcastResponder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Broadcasts a query and collects replies for `timeout`.
pub(crate) async fn probe(timeout: Duration) -> Result<Vec<Peer>, Error> {
    probe_port(timeout, DISCOVERY_PORT).await
}

async fn probe_port(timeout: Duration, port: u16) -> Result<Vec<Peer>, Error> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let query = [MAGIC[0], MAGIC[1], MAGIC[2], MAGIC[3], QUERY];
    for target in broadcast_targets() {
        if let Err(e) = socket.send_to(&query, SocketAddr::new(target.into(), port)).await {
            tracing::debug!("broadcast to {target} failed: {e}");
        }
    }
    let mut peers = Vec::new();
    let mut buf = [0u8; 1024];
    let deadline = tokio::time::Instant::now() + timeout;
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, socket.recv_from(&mut buf)).await {
        if let Some(peer) = decode_announce(&buf[..n], from.ip()) {
            peers.push(peer);
        }
    }
    Ok(peers)
}

/// Limited broadcast, each interface's directed broadcast, and loopback so a
/// host on the same machine answers too.
fn broadcast_targets() -> Vec<Ipv4Addr> {
    let mut targets = vec![Ipv4Addr::BROADCAST, Ipv4Addr::LOCALHOST];
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if let Some(b) = v4.broadcast {
                if !targets.contains(&b) {
                    targets.push(b);
                }
            }
        }
    }
    targets
}

/// UDP socket that several processes may bind (a second Okno instance or
/// a test must not fail to start).
pub(crate) fn shared_socket(addr: SocketAddr) -> Result<UdpSocket, Error> {
    let socket = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_broadcast(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    Ok(UdpSocket::from_std(socket.into())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> HostAnnouncement {
        HostAnnouncement { name: "Desk".into(), os: "linux".into(), fingerprint: Fingerprint([7; 32]), port: 21200 }
    }

    #[test]
    fn announce_round_trip() {
        let from: IpAddr = "192.168.1.20".parse().unwrap();
        let peer = decode_announce(&encode_announce(&info()), from).unwrap();
        assert_eq!(peer.name, "Desk");
        assert_eq!(peer.fingerprint, Fingerprint([7; 32]));
        assert_eq!(peer.addresses, vec!["192.168.1.20:21200".parse().unwrap()]);
    }

    #[test]
    fn rejects_garbage() {
        let from: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(decode_announce(b"OKNO\x02\xff\xff", from).is_none());
        assert!(decode_announce(b"XXXX\x02", from).is_none());
        assert!(is_query(b"OKNO\x01"));
        assert!(!is_query(b"OKNO\x02"));
    }

    #[tokio::test]
    async fn probe_finds_local_responder() {
        // Private port so parallel test runs do not answer each other.
        let port = 41_000 + (std::process::id() % 5000) as u16;
        let socket = shared_socket(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))).unwrap();
        let reply = encode_announce(&info());
        tokio::spawn(async move {
            let mut buf = [0u8; 64];
            loop {
                let (n, from) = socket.recv_from(&mut buf).await.unwrap();
                if is_query(&buf[..n]) {
                    socket.send_to(&reply, from).await.unwrap();
                }
            }
        });
        let peers = probe_port(Duration::from_millis(300), port).await.unwrap();
        assert!(peers.iter().any(|p| p.fingerprint == Fingerprint([7; 32])));
    }
}
