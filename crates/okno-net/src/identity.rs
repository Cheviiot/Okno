use std::fmt;

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::NOISE_PARAMS;

/// Long-term X25519 key of a device. The public half identifies the device;
/// peers pin its [`Fingerprint`].
pub struct Identity {
    private: Zeroizing<Vec<u8>>,
    public: [u8; 32],
}

impl Identity {
    pub fn generate() -> Self {
        let keypair = snow::Builder::new(NOISE_PARAMS.parse().expect("valid Noise params"))
            .generate_keypair()
            .expect("key generation cannot fail with a system RNG");
        Self::from_parts(keypair.private, &keypair.public)
    }

    /// Restores an identity from its 32-byte private key.
    pub fn from_private(private: &[u8]) -> Option<Self> {
        let private: [u8; 32] = private.try_into().ok()?;
        let public = x25519_public(&private);
        Some(Self::from_parts(private.to_vec(), &public))
    }

    fn from_parts(private: Vec<u8>, public: &[u8]) -> Self {
        Self { private: Zeroizing::new(private), public: public.try_into().expect("X25519 public key is 32 bytes") }
    }

    pub fn private_key(&self) -> &[u8] {
        &self.private
    }

    pub fn public_key(&self) -> &[u8; 32] {
        &self.public
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.public)
    }
}

impl fmt::Debug for Identity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Identity").field("fingerprint", &self.fingerprint()).finish_non_exhaustive()
    }
}

/// SHA-256 of a device's static public key.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Fingerprint(pub [u8; 32]);

impl Fingerprint {
    pub fn of(public_key: &[u8]) -> Self {
        Self(Sha256::digest(public_key).into())
    }

    /// Full lowercase hex, the stored and transmitted form.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(text.trim(), &mut bytes).ok()?;
        Some(Self(bytes))
    }

    /// Short form for people to compare: first 16 bytes in groups of four
    /// hex digits, e.g. `3f2a 9c01 …`.
    pub fn display_short(&self) -> String {
        let hex = hex::encode_upper(&self.0[..16]);
        hex.as_bytes().chunks(4).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join(" ")
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({})", &self.to_hex()[..16])
    }
}

/// Derives the X25519 public key through snow's crypto resolver.
fn x25519_public(private: &[u8; 32]) -> [u8; 32] {
    use snow::resolvers::{CryptoResolver, DefaultResolver};
    let mut dh = DefaultResolver.resolve_dh(&snow::params::DHChoice::Curve25519).expect("Curve25519 available");
    dh.set(private);
    dh.pubkey().try_into().expect("X25519 public key is 32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_same_public_key() {
        let id = Identity::generate();
        let restored = Identity::from_private(id.private_key()).unwrap();
        assert_eq!(id.public_key(), restored.public_key());
        assert_eq!(id.fingerprint(), restored.fingerprint());
    }

    #[test]
    fn fingerprint_hex_round_trip() {
        let fp = Identity::generate().fingerprint();
        assert_eq!(Fingerprint::from_hex(&fp.to_hex()), Some(fp));
        assert_eq!(fp.display_short().len(), 32 + 7);
        assert!(Fingerprint::from_hex("zz").is_none());
    }
}
