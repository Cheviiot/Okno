//! Okno wire messages.
//!
//! Messages are declared with `prost` derives directly instead of `.proto`
//! files, so the build needs no `protoc`. The schema is documented in
//! `docs/protocol.md`; field tags must never be reused.

mod messages;

pub use messages::*;
use prost::Message as _;

/// Protocol version carried in [`Hello`]. Peers with a different version refuse
/// the session instead of negotiating.
pub const PROTOCOL_VERSION: u32 = 2;

/// Default TCP port of a host.
pub const DEFAULT_PORT: u16 = 21200;

/// UDP port of the broadcast discovery.
pub const DISCOVERY_PORT: u16 = 21201;

/// mDNS service type.
pub const MDNS_SERVICE: &str = "_okno._tcp.local.";

/// Noise prologue; binds the handshake to this protocol.
pub const NOISE_PROLOGUE: &[u8] = b"okno/1";

/// Largest encoded application message accepted from a peer.
pub const MAX_MESSAGE_LEN: usize = 32 * 1024 * 1024;

/// Logical channels. A message travels on one channel; the transport sends
/// channels with a lower number first, so a large file chunk never delays
/// input or video.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Channel {
    Control = 0,
    Input = 1,
    Audio = 2,
    Video = 3,
    Bulk = 4,
}

impl Channel {
    pub const COUNT: usize = 5;

    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => Self::Control,
            1 => Self::Input,
            2 => Self::Audio,
            3 => Self::Video,
            4 => Self::Bulk,
            _ => return None,
        })
    }

    /// Channel a message belongs to.
    pub fn of(message: &envelope::Msg) -> Self {
        use envelope::Msg;
        match message {
            // Keystrokes and echo must not wait behind file transfers.
            Msg::Input(_) | Msg::Terminal(_) => Self::Input,
            Msg::Audio(_) => Self::Audio,
            Msg::Video(_) => Self::Video,
            Msg::File(_) | Msg::Tunnel(_) => Self::Bulk,
            _ => Self::Control,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("malformed message: {0}")]
    Prost(#[from] prost::DecodeError),
    #[error("message has no payload")]
    Empty,
}

impl Envelope {
    pub fn new(msg: envelope::Msg) -> Self {
        Self { msg: Some(msg) }
    }

    pub fn encode_vec(&self) -> Vec<u8> {
        self.encode_to_vec()
    }

    pub fn decode_msg(bytes: &[u8]) -> Result<envelope::Msg, DecodeError> {
        Envelope::decode(bytes)?.msg.ok_or(DecodeError::Empty)
    }
}

impl From<envelope::Msg> for Envelope {
    fn from(msg: envelope::Msg) -> Self {
        Self::new(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_round_trip() {
        let msg = envelope::Msg::Hello(Hello {
            version: PROTOCOL_VERSION,
            device_name: "desk".into(),
            os: "linux".into(),
            capabilities: vec!["video".into()],
        });
        let bytes = Envelope::new(msg.clone()).encode_vec();
        assert_eq!(Envelope::decode_msg(&bytes).unwrap(), msg);
    }

    #[test]
    fn channels_are_ordered_by_priority() {
        let video = envelope::Msg::Video(VideoFrame::default());
        let input = envelope::Msg::Input(InputEvent::default());
        assert!(Channel::of(&input) < Channel::of(&video));
        assert_eq!(Channel::from_u8(4), Some(Channel::Bulk));
        assert_eq!(Channel::from_u8(9), None);
    }

    #[test]
    fn empty_envelope_is_rejected() {
        assert!(matches!(Envelope::decode_msg(&[]), Err(DecodeError::Empty)));
    }
}
