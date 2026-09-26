//! Settings and keys on disk.
//!
//! Everything lives in one directory: `$XDG_CONFIG_HOME/okno` on Linux,
//! `%APPDATA%\okno` on Windows, or `$OKNO_CONFIG_DIR` when set.
//!
//! - `config.toml` — [`Config`]
//! - `trust.toml` — pinned host keys ([`TrustStore`])
//! - `identity.key` — 32-byte private key of this device

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use okno_auth::{AllowList, Credentials, TrustStore};
use okno_net::Identity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device_name: String,
    pub host: HostConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self { device_name: crate::default_device_name(), host: HostConfig::default() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// Accept incoming connections.
    pub enabled: bool,
    pub port: u16,
    /// Addresses to listen on; empty means all.
    pub listen: Vec<IpAddr>,
    pub allowed_networks: AllowList,
    /// Announce the host via mDNS and answer broadcast queries.
    pub discoverable: bool,
    pub credentials: Option<Credentials>,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: okno_proto::DEFAULT_PORT,
            listen: Vec::new(),
            allowed_networks: AllowList::default(),
            discoverable: true,
            credentials: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cannot find a configuration directory")]
    NoConfigDir,
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("{path}: {source}")]
    Parse { path: PathBuf, source: toml::de::Error },
    #[error("{0}: invalid device key")]
    BadKey(PathBuf),
}

/// The configuration directory.
#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn open_default() -> Result<Self, StoreError> {
        let dir = match std::env::var_os("OKNO_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir().ok_or(StoreError::NoConfigDir)?.join("okno"),
        };
        Ok(Self::at(dir))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn load_config(&self) -> Result<Config, StoreError> {
        self.load_toml("config.toml")
    }

    pub fn save_config(&self, config: &Config) -> Result<(), StoreError> {
        self.save_toml("config.toml", config)
    }

    pub fn load_trust(&self) -> Result<TrustStore, StoreError> {
        self.load_toml("trust.toml")
    }

    pub fn save_trust(&self, trust: &TrustStore) -> Result<(), StoreError> {
        self.save_toml("trust.toml", trust)
    }

    /// Loads this device's key, creating it on first run.
    pub fn identity(&self) -> Result<Identity, StoreError> {
        let path = self.dir.join("identity.key");
        match std::fs::read(&path) {
            Ok(bytes) => Identity::from_private(&bytes).ok_or(StoreError::BadKey(path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let identity = Identity::generate();
                self.write_private("identity.key", identity.private_key())?;
                Ok(identity)
            }
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    fn load_toml<T: Default + for<'de> Deserialize<'de>>(&self, name: &str) -> Result<T, StoreError> {
        let path = self.dir.join(name);
        match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|source| StoreError::Parse { path, source }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(source) => Err(StoreError::Io { path, source }),
        }
    }

    fn save_toml<T: Serialize>(&self, name: &str, value: &T) -> Result<(), StoreError> {
        let text = toml::to_string_pretty(value).expect("config serialises");
        // The config holds the password hash, so keep it private too.
        self.write_private(name, text.as_bytes())
    }

    /// Writes atomically (temp file + rename) with owner-only permissions.
    fn write_private(&self, name: &str, data: &[u8]) -> Result<(), StoreError> {
        let path = self.dir.join(name);
        let io = |source| StoreError::Io { path: path.clone(), source };
        std::fs::create_dir_all(&self.dir).map_err(io)?;
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp).map_err(io)?;
        file.write_all(data).map_err(io)?;
        file.sync_all().map_err(io)?;
        drop(file);
        std::fs::rename(&tmp, &path).map_err(io)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trip_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let mut config = store.load_config().unwrap();
        assert_eq!(config.host.port, 21200);
        assert!(!config.host.enabled);
        config.host.enabled = true;
        config.host.credentials = Some(Credentials::new("admin", "password1").unwrap());
        store.save_config(&config).unwrap();
        assert_eq!(store.load_config().unwrap(), config);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "device_name = \"x\"\n[host]\nport = 1\n").unwrap();
        let config = Store::at(dir.path()).load_config().unwrap();
        assert_eq!(config.device_name, "x");
        assert_eq!(config.host.port, 1);
        assert!(config.host.discoverable);
    }

    #[test]
    fn identity_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("nested"));
        let a = store.identity().unwrap();
        let b = store.identity().unwrap();
        assert_eq!(a.fingerprint(), b.fingerprint());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(store.dir().join("identity.key")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
