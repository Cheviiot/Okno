//! mDNS/DNS-SD advertising of `_okno._tcp`.
//!
//! TXT records: `v` protocol version, `name` display name, `os`, `fp`
//! fingerprint hex. The instance label is `<name>-<fp prefix>` so two hosts
//! with the same name do not collide.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use okno_net::Fingerprint;
use okno_proto::MDNS_SERVICE;

use crate::{Error, HostAnnouncement, Peer, Source, acceptable};

/// Registers the host with mDNS until dropped.
pub struct MdnsAdvertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl MdnsAdvertiser {
    pub fn start(info: &HostAnnouncement) -> Result<Self, Error> {
        let daemon = ServiceDaemon::new()?;
        let fp = info.fingerprint.to_hex();
        let instance = format!("{}-{}", label(&info.name), &fp[..8]);
        let hostname = format!("okno-{}.local.", &fp[..12]);
        let mut props = HashMap::new();
        props.insert("v".to_owned(), okno_proto::PROTOCOL_VERSION.to_string());
        props.insert("name".to_owned(), info.name.clone());
        props.insert("os".to_owned(), info.os.clone());
        props.insert("fp".to_owned(), fp);
        let service = ServiceInfo::new(MDNS_SERVICE, &instance, &hostname, "", info.port, props)?.enable_addr_auto();
        let fullname = service.get_fullname().to_owned();
        daemon.register(service)?;
        Ok(Self { daemon, fullname })
    }
}

impl Drop for MdnsAdvertiser {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// DNS labels are limited to 63 bytes; keep the readable part short.
fn label(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '-' }).collect();
    let mut out = String::new();
    for c in cleaned.trim_matches('-').chars() {
        if out.len() + c.len_utf8() > 40 {
            break;
        }
        out.push(c);
    }
    if out.is_empty() { "okno".into() } else { out }
}

pub(crate) async fn browse(timeout: Duration) -> Result<Vec<Peer>, Error> {
    tokio::task::spawn_blocking(move || browse_blocking(timeout)).await.expect("mDNS browse thread panicked")
}

fn browse_blocking(timeout: Duration) -> Result<Vec<Peer>, Error> {
    let daemon = ServiceDaemon::new()?;
    let events = daemon.browse(MDNS_SERVICE)?;
    let deadline = Instant::now() + timeout;
    let mut peers = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = events.recv_timeout(left) else { break };
        if let ServiceEvent::ServiceResolved(info) = event {
            if let Some(peer) = to_peer(&info) {
                peers.push(peer);
            }
        }
    }
    let _ = daemon.stop_browse(MDNS_SERVICE);
    let _ = daemon.shutdown();
    Ok(peers)
}

fn to_peer(info: &ServiceInfo) -> Option<Peer> {
    let version = info.get_property_val_str("v")?.parse().ok()?;
    let name = info.get_property_val_str("name")?.to_owned();
    if !acceptable(version, &name) {
        return None;
    }
    let fingerprint = Fingerprint::from_hex(info.get_property_val_str("fp")?)?;
    let port = info.get_port();
    let addresses: Vec<SocketAddr> = info
        .get_addresses()
        .iter()
        // Link-local IPv6 without a scope id cannot be dialled.
        .filter(|ip| !matches!(ip, std::net::IpAddr::V6(v6) if v6.is_unicast_link_local()))
        .map(|ip| SocketAddr::new(*ip, port))
        .collect();
    (!addresses.is_empty()).then(|| Peer {
        name,
        os: info.get_property_val_str("os").unwrap_or_default().to_owned(),
        fingerprint,
        addresses,
        sources: vec![Source::Mdns],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_dns_safe() {
        assert_eq!(label("Рабочий ПК"), "Рабочий-ПК");
        assert_eq!(label("  "), "okno");
        assert!(label(&"x".repeat(100)).len() <= 40);
        assert!(label(&"ы".repeat(100)).len() <= 40);
    }
}
