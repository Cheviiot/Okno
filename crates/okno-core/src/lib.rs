//! Okno sessions without any UI: the host side that accepts connections and
//! the client side that opens them. Frontends (the Slint app, `okno-cli`)
//! drive these types and render their events.

pub mod client;
pub mod config;
pub mod endpoint;
pub mod host;

pub use config::{Config, HostConfig, Store};
pub use endpoint::Endpoint;

/// `linux` or `windows`, as sent in `Hello` and discovery.
pub fn os_name() -> &'static str {
    if cfg!(windows) { "windows" } else { std::env::consts::OS }
}

/// A sensible default device name: the machine's host name.
pub fn default_device_name() -> String {
    let name = std::env::var("COMPUTERNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_default();
    let name = name.trim();
    if name.is_empty() { "Okno".into() } else { name.to_owned() }
}
