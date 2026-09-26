use std::net::IpAddr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

/// Networks allowed to connect to the host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AllowList(pub Vec<IpNet>);

impl Default for AllowList {
    /// Loopback, private, link-local and CGNAT (Tailscale) ranges: everything a
    /// LAN or an overlay VPN uses, nothing routed from the internet.
    fn default() -> Self {
        Self(
            [
                "127.0.0.0/8",
                "10.0.0.0/8",
                "172.16.0.0/12",
                "192.168.0.0/16",
                "169.254.0.0/16",
                "100.64.0.0/10",
                "::1/128",
                "fc00::/7",
                "fe80::/10",
            ]
            .iter()
            .map(|n| n.parse().unwrap())
            .collect(),
        )
    }
}

impl AllowList {
    pub fn parse<'a>(items: impl IntoIterator<Item = &'a str>) -> Result<Self, ipnet::AddrParseError> {
        items
            .into_iter()
            .map(|s| {
                let s = s.trim();
                // Accept bare addresses as single-host networks.
                s.parse::<IpNet>().or_else(|e| s.parse::<IpAddr>().map(IpNet::from).map_err(|_| e))
            })
            .collect::<Result<_, _>>()
            .map(Self)
    }

    pub fn allows(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical();
        self.0.iter().any(|net| net.contains(&ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_allows_lan_only() {
        let list = AllowList::default();
        for ip in ["192.168.1.10", "10.1.2.3", "100.100.1.1", "::1", "fd12::1", "::ffff:192.168.0.2"] {
            assert!(list.allows(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "2001:db8::1", "172.32.0.1"] {
            assert!(!list.allows(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn parses_hosts_and_networks() {
        let list = AllowList::parse(["192.168.5.0/24", " 10.0.0.7 "]).unwrap();
        assert!(list.allows("192.168.5.99".parse().unwrap()));
        assert!(list.allows("10.0.0.7".parse().unwrap()));
        assert!(!list.allows("10.0.0.8".parse().unwrap()));
        assert!(AllowList::parse(["nonsense"]).is_err());
    }
}
