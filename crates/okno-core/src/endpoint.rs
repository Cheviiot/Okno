use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

use okno_proto::DEFAULT_PORT;

/// A host address as a user types it: `name`, `name:port`, `192.168.1.5`,
/// `fd00::1` or `[fd00::1]:21200`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid address “{0}”")]
pub struct BadEndpoint(pub String);

impl FromStr for Endpoint {
    type Err = BadEndpoint;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let s = input.trim();
        let bad = || BadEndpoint(input.to_owned());
        if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '/') {
            return Err(bad());
        }
        if let Ok(addr) = s.parse::<SocketAddr>() {
            return Ok(Self { host: addr.ip().to_string(), port: addr.port() });
        }
        if let Ok(ip) = s.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
            return Ok(Self { host: ip.to_string(), port: DEFAULT_PORT });
        }
        let (host, port) = match s.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad)?),
            None => (s, DEFAULT_PORT),
        };
        let valid_name = !host.is_empty()
            && host.len() <= 253
            && host.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '.' | '_'));
        if !valid_name {
            return Err(bad());
        }
        Ok(Self { host: host.to_ascii_lowercase(), port })
    }
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.contains(':') {
            write!(f, "[{}]:{}", self.host, self.port)
        } else {
            write!(f, "{}:{}", self.host, self.port)
        }
    }
}

impl From<SocketAddr> for Endpoint {
    fn from(addr: SocketAddr) -> Self {
        Self { host: addr.ip().to_string(), port: addr.port() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(s: &str) -> String {
        s.parse::<Endpoint>().unwrap().to_string()
    }

    #[test]
    fn parses_user_input() {
        assert_eq!(ep("desk"), "desk:21200");
        assert_eq!(ep("Desk.local:9000"), "desk.local:9000");
        assert_eq!(ep("192.168.1.5"), "192.168.1.5:21200");
        assert_eq!(ep("192.168.1.5:1"), "192.168.1.5:1");
        assert_eq!(ep("fd00::1"), "[fd00::1]:21200");
        assert_eq!(ep("[fd00::1]"), "[fd00::1]:21200");
        assert_eq!(ep("[fd00::1]:22"), "[fd00::1]:22");
    }

    #[test]
    fn rejects_nonsense() {
        for s in ["", "a b", "desk:0", "desk:99999", "http://x", "desk:", "ho$t"] {
            assert!(s.parse::<Endpoint>().is_err(), "{s}");
        }
    }
}
