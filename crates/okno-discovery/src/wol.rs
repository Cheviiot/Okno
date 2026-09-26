use std::fmt;
use std::net::{Ipv4Addr, SocketAddr};
use std::str::FromStr;

use tokio::net::UdpSocket;

/// A 48-bit hardware address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MacAddress(pub [u8; 6]);

impl FromStr for MacAddress {
    type Err = ();

    /// Accepts `aa:bb:cc:dd:ee:ff`, `aa-bb-…` and `aabbccddeeff`.
    fn from_str(s: &str) -> Result<Self, ()> {
        let hex: String = s.chars().filter(|c| !matches!(c, ':' | '-' | '.')).collect();
        if hex.len() != 12 || !hex.is_ascii() {
            return Err(());
        }
        let mut mac = [0u8; 6];
        for (i, byte) in mac.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).map_err(|_| ())?;
        }
        Ok(Self(mac))
    }
}

impl fmt::Display for MacAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}

fn magic_packet(mac: MacAddress) -> [u8; 102] {
    let mut packet = [0xFF; 102];
    for chunk in packet[6..].chunks_mut(6) {
        chunk.copy_from_slice(&mac.0);
    }
    packet
}

/// Sends the Wake-on-LAN magic packet to UDP port 9 on every broadcast
/// address. Returns how many sends succeeded.
pub async fn send_magic_packet(mac: MacAddress) -> std::io::Result<usize> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;
    socket.set_broadcast(true)?;
    let packet = magic_packet(mac);
    let mut targets = vec![Ipv4Addr::BROADCAST];
    for iface in if_addrs::get_if_addrs().unwrap_or_default() {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if let Some(b) = v4.broadcast.filter(|b| !targets.contains(b)) {
                targets.push(b);
            }
        }
    }
    let mut sent = 0;
    let mut last_err = None;
    for target in targets {
        match socket.send_to(&packet, SocketAddr::new(target.into(), 9)).await {
            Ok(_) => sent += 1,
            Err(e) => last_err = Some(e),
        }
    }
    match (sent, last_err) {
        (0, Some(e)) => Err(e),
        _ => Ok(sent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_formats() {
        let mac = MacAddress([0xaa, 0xbb, 0xcc, 0x01, 0x02, 0x03]);
        for s in ["aa:bb:cc:01:02:03", "AA-BB-CC-01-02-03", "aabbcc010203"] {
            assert_eq!(s.parse::<MacAddress>(), Ok(mac), "{s}");
        }
        assert_eq!(mac.to_string(), "aa:bb:cc:01:02:03");
        assert!("aa:bb".parse::<MacAddress>().is_err());
        assert!("zz:bb:cc:01:02:03".parse::<MacAddress>().is_err());
    }

    #[test]
    fn magic_packet_layout() {
        let mac = MacAddress([1, 2, 3, 4, 5, 6]);
        let p = magic_packet(mac);
        assert_eq!(&p[..6], &[0xFF; 6]);
        assert!(p[6..].chunks(6).all(|c| c == [1, 2, 3, 4, 5, 6]));
    }
}
