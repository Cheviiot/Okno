use std::collections::BTreeMap;

use okno_net::Fingerprint;
use serde::{Deserialize, Serialize};

/// A host key the user accepted.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedDevice {
    pub name: String,
    /// Endpoints (`host:port`) this key was seen at.
    pub endpoints: Vec<String>,
    /// Unix seconds.
    pub first_seen: i64,
    /// Hardware addresses the host reported, for Wake-on-LAN.
    #[serde(default)]
    pub macs: Vec<String>,
}

/// Result of comparing a host's key with what the client pinned before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustDecision {
    /// Known key at a known endpoint.
    Trusted,
    /// Known key at a new endpoint (DHCP moved it); safe to accept.
    KnownElsewhere,
    /// Never seen: ask the user to compare fingerprints.
    New,
    /// The endpoint belonged to another key: warn loudly.
    Changed { previous: Fingerprint },
}

/// Trust-on-first-use store of host keys, keyed by fingerprint hex.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub devices: BTreeMap<String, TrustedDevice>,
}

impl TrustStore {
    pub fn check(&self, endpoint: &str, fp: &Fingerprint) -> TrustDecision {
        let endpoint = normalize(endpoint);
        if let Some(device) = self.devices.get(&fp.to_hex()) {
            return if device.endpoints.contains(&endpoint) {
                TrustDecision::Trusted
            } else {
                TrustDecision::KnownElsewhere
            };
        }
        match self.owner_of(&endpoint) {
            Some(previous) => TrustDecision::Changed { previous },
            None => TrustDecision::New,
        }
    }

    /// Pins `fp` for `endpoint`, moving the endpoint away from any other key.
    pub fn trust(&mut self, endpoint: &str, fp: &Fingerprint, name: &str, now_unix: i64) {
        let endpoint = normalize(endpoint);
        for device in self.devices.values_mut() {
            device.endpoints.retain(|e| *e != endpoint);
        }
        let device = self
            .devices
            .entry(fp.to_hex())
            .or_insert_with(|| TrustedDevice { first_seen: now_unix, ..Default::default() });
        if !name.is_empty() {
            device.name = name.to_owned();
        }
        device.endpoints.push(endpoint);
    }

    /// Records the host's hardware addresses for Wake-on-LAN.
    pub fn set_macs(&mut self, fp: &Fingerprint, macs: Vec<String>) {
        if let Some(device) = self.devices.get_mut(&fp.to_hex()) {
            device.macs = macs;
        }
    }

    pub fn forget(&mut self, fp: &Fingerprint) -> bool {
        self.devices.remove(&fp.to_hex()).is_some()
    }

    fn owner_of(&self, endpoint: &str) -> Option<Fingerprint> {
        self.devices
            .iter()
            .find(|(_, d)| d.endpoints.iter().any(|e| e == endpoint))
            .and_then(|(hex, _)| Fingerprint::from_hex(hex))
    }
}

fn normalize(endpoint: &str) -> String {
    endpoint.trim().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(b: u8) -> Fingerprint {
        Fingerprint([b; 32])
    }

    #[test]
    fn tofu_flow() {
        let mut store = TrustStore::default();
        assert_eq!(store.check("desk:21200", &fp(1)), TrustDecision::New);
        store.trust("desk:21200", &fp(1), "Desk", 100);
        assert_eq!(store.check("DESK:21200", &fp(1)), TrustDecision::Trusted);
        assert_eq!(store.check("192.168.1.9:21200", &fp(1)), TrustDecision::KnownElsewhere);
        assert_eq!(store.check("desk:21200", &fp(2)), TrustDecision::Changed { previous: fp(1) });
    }

    #[test]
    fn accepting_new_key_moves_endpoint() {
        let mut store = TrustStore::default();
        store.trust("desk:21200", &fp(1), "Desk", 100);
        store.trust("desk:21200", &fp(2), "Desk", 200);
        assert_eq!(store.check("desk:21200", &fp(2)), TrustDecision::Trusted);
        assert_eq!(store.check("desk:21200", &fp(1)), TrustDecision::KnownElsewhere);
        assert_eq!(store.devices[&fp(2).to_hex()].first_seen, 200);
        assert!(store.forget(&fp(1)));
        assert!(!store.forget(&fp(1)));
    }
}
