//! Finding Okno hosts on the local network and waking them up.
//!
//! Two mechanisms run side by side because each fails somewhere: mDNS is
//! filtered on some Wi-Fi networks and by Windows firewall profiles, while
//! broadcast does not cross subnets that mDNS reflectors bridge.

mod broadcast;
mod mdns;
mod wol;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use okno_net::Fingerprint;

pub use broadcast::BroadcastResponder;
pub use mdns::MdnsAdvertiser;
pub use wol::{MacAddress, send_magic_packet};

/// What a host tells the network about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAnnouncement {
    pub name: String,
    /// `linux` or `windows`.
    pub os: String,
    pub fingerprint: Fingerprint,
    pub port: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    Mdns,
    Broadcast,
}

/// A host seen on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub name: String,
    pub os: String,
    pub fingerprint: Fingerprint,
    pub addresses: Vec<SocketAddr>,
    pub sources: Vec<Source>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("mDNS error: {0}")]
    Mdns(#[from] mdns_sd::Error),
}

/// Keeps a host visible while alive.
pub struct Announcer {
    _mdns: Option<MdnsAdvertiser>,
    _broadcast: Option<BroadcastResponder>,
}

impl Announcer {
    /// Starts both advertisers. Either may fail (port taken, no multicast);
    /// the host stays reachable by address, so failures are only logged.
    pub async fn start(info: HostAnnouncement) -> Self {
        let mdns =
            MdnsAdvertiser::start(&info).inspect_err(|e| tracing::warn!("mDNS advertising unavailable: {e}")).ok();
        let broadcast = BroadcastResponder::start(info)
            .await
            .inspect_err(|e| tracing::warn!("broadcast discovery unavailable: {e}"))
            .ok();
        Self { _mdns: mdns, _broadcast: broadcast }
    }
}

/// Looks for hosts for `timeout`, merging results by fingerprint and leaving
/// out `own` (this device).
pub async fn discover(timeout: Duration, own: Option<Fingerprint>) -> Vec<Peer> {
    let (mdns, bcast) = tokio::join!(mdns::browse(timeout), broadcast::probe(timeout));
    let mut found: Vec<(Source, Peer)> = Vec::new();
    match mdns {
        Ok(peers) => found.extend(peers.into_iter().map(|p| (Source::Mdns, p))),
        Err(e) => tracing::debug!("mDNS browse failed: {e}"),
    }
    match bcast {
        Ok(peers) => found.extend(peers.into_iter().map(|p| (Source::Broadcast, p))),
        Err(e) => tracing::debug!("broadcast probe failed: {e}"),
    }
    merge(found, own)
}

fn merge(found: Vec<(Source, Peer)>, own: Option<Fingerprint>) -> Vec<Peer> {
    let mut by_fp: BTreeMap<Fingerprint, Peer> = BTreeMap::new();
    for (source, peer) in found {
        if Some(peer.fingerprint) == own {
            continue;
        }
        let entry = by_fp.entry(peer.fingerprint).or_insert_with(|| Peer {
            addresses: Vec::new(),
            sources: Vec::new(),
            ..peer.clone()
        });
        for addr in peer.addresses {
            if !entry.addresses.contains(&addr) {
                entry.addresses.push(addr);
            }
        }
        if !entry.sources.contains(&source) {
            entry.sources.push(source);
        }
    }
    let mut peers: Vec<Peer> = by_fp.into_values().collect();
    for peer in &mut peers {
        // IPv4 first: link-local IPv6 needs a scope id that users cannot type.
        peer.addresses.sort_by_key(|a| (a.is_ipv6(), *a));
        peer.sources.sort();
    }
    peers.sort_by_key(|p| p.name.to_lowercase());
    peers
}

/// Protocol-level sanity filter shared by both mechanisms.
pub(crate) fn acceptable(version: u32, name: &str) -> bool {
    version == okno_proto::PROTOCOL_VERSION && !name.is_empty() && name.len() <= 128
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(fp: u8, name: &str, addr: &str) -> Peer {
        Peer {
            name: name.into(),
            os: "linux".into(),
            fingerprint: Fingerprint([fp; 32]),
            addresses: vec![addr.parse().unwrap()],
            sources: vec![],
        }
    }

    #[test]
    fn merges_by_fingerprint_and_skips_self() {
        let found = vec![
            (Source::Mdns, peer(1, "beta", "[fd00::5]:21200")),
            (Source::Broadcast, peer(1, "beta", "192.168.1.5:21200")),
            (Source::Broadcast, peer(2, "Alpha", "192.168.1.6:21200")),
            (Source::Mdns, peer(3, "me", "192.168.1.7:21200")),
        ];
        let peers = merge(found, Some(Fingerprint([3; 32])));
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].name, "Alpha");
        assert_eq!(peers[1].addresses[0], "192.168.1.5:21200".parse().unwrap());
        assert_eq!(peers[1].sources, vec![Source::Mdns, Source::Broadcast]);
    }
}
