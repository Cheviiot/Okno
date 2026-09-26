use std::io::BufRead;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use okno_auth::{Credentials, TrustDecision};
use okno_core::client::{self, ClientError};
use okno_core::host::{ControlOnly, Host, HostEvent, HostSettings};
use okno_core::{Endpoint, Store};
use okno_discovery::MacAddress;

/// Okno without a window: run a host, test a connection, find devices.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show this device's name and fingerprint.
    Id,
    /// Set the host login. The password is read from $OKNO_PASSWORD or the
    /// first line of stdin.
    SetPassword {
        #[arg(long, default_value = "okno")]
        user: String,
    },
    /// Accept connections until interrupted.
    Host {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Look for hosts on the local network.
    Discover {
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
    /// Connect, log in and measure latency.
    Connect {
        endpoint: String,
        #[arg(long, default_value = "okno")]
        user: String,
        /// Accept a new or changed host key without asking.
        #[arg(long)]
        trust: bool,
        #[arg(long, default_value_t = 3)]
        pings: u32,
    },
    /// Send a Wake-on-LAN packet.
    Wake { mac: String },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_env("OKNO_LOG").unwrap_or_else(|_| "warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let store = Store::open_default()?;
    match cli.command {
        Command::Id => {
            let config = store.load_config()?;
            let identity = store.identity()?;
            println!("name:        {}", config.device_name);
            println!("fingerprint: {}", identity.fingerprint().display_short());
            println!("full:        {}", identity.fingerprint());
        }
        Command::SetPassword { user } => {
            let password = read_password()?;
            let mut config = store.load_config()?;
            config.host.credentials = Some(Credentials::new(&user, &password)?);
            config.host.enabled = true;
            store.save_config(&config)?;
            println!("host login set for “{user}”");
        }
        Command::Host { port } => run_host(&store, port).await?,
        Command::Discover { seconds } => {
            let own = store.identity()?.fingerprint();
            let peers = okno_discovery::discover(Duration::from_secs(seconds), Some(own)).await;
            if peers.is_empty() {
                println!("no hosts found");
            }
            for p in peers {
                let addrs: Vec<String> = p.addresses.iter().map(ToString::to_string).collect();
                println!("{}  [{}]  {}  {}", p.name, p.os, p.fingerprint.display_short(), addrs.join(", "));
            }
        }
        Command::Connect { endpoint, user, trust, pings } => {
            connect(&store, &endpoint, &user, trust, pings).await?;
        }
        Command::Wake { mac } => {
            let mac: MacAddress = mac.parse().map_err(|_| anyhow::anyhow!("invalid MAC address"))?;
            let sent = okno_discovery::send_magic_packet(mac).await?;
            println!("magic packet for {mac} sent to {sent} broadcast address(es)");
        }
    }
    Ok(())
}

fn read_password() -> Result<String> {
    if let Ok(p) = std::env::var("OKNO_PASSWORD") {
        return Ok(p);
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

async fn run_host(store: &Store, port: Option<u16>) -> Result<()> {
    let config = store.load_config()?;
    let credentials =
        config.host.credentials.clone().context("no host login configured; run `okno-cli set-password` first")?;
    let settings = HostSettings {
        device_name: config.device_name.clone(),
        port: port.unwrap_or(config.host.port),
        listen: config.host.listen.clone(),
        allowed_networks: config.host.allowed_networks.clone(),
        credentials,
        discoverable: config.host.discoverable,
        services: Vec::new(),
    };
    let identity = store.identity()?;
    println!("fingerprint: {}", identity.fingerprint().display_short());
    let host = Host::start(identity, settings, Arc::new(ControlOnly)).await?;
    let mut events = host.subscribe();
    for addr in host.local_addrs() {
        println!("listening on {addr}");
    }
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(HostEvent::SessionOpened(s)) => println!("session {} from {} ({}, {})", s.id, s.peer, s.device_name, s.fingerprint.display_short()),
                Ok(HostEvent::SessionClosed { id, reason }) => println!("session {id} ended: {reason}"),
                Ok(HostEvent::LoginFailed { peer, status }) => println!("login from {peer} rejected: {status:?}"),
                Ok(HostEvent::Refused { peer }) => println!("refused {peer} (not in allowed networks)"),
                Ok(HostEvent::Listening(_)) | Err(_) => {}
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

async fn connect(store: &Store, endpoint: &str, user: &str, accept_key: bool, pings: u32) -> Result<()> {
    let endpoint: Endpoint = endpoint.parse()?;
    let config = store.load_config()?;
    let identity = store.identity()?;
    let mut trust = store.load_trust()?;
    let mut pending = client::open(&endpoint, &identity, &trust, &config.device_name).await?;
    println!("host: {} [{}]", pending.host.device_name, pending.host.os);
    println!("fingerprint: {}", pending.fingerprint.display_short());
    match &pending.trust {
        TrustDecision::Trusted => {}
        TrustDecision::KnownElsewhere => println!("known device at a new address"),
        TrustDecision::New if !accept_key => bail!("new device; compare the fingerprint and rerun with --trust"),
        TrustDecision::Changed { previous } if !accept_key => bail!(
            "WARNING: the key of {endpoint} changed (was {}); rerun with --trust only if you expected this",
            previous.display_short()
        ),
        TrustDecision::New | TrustDecision::Changed { .. } => {}
    }
    let password = read_password()?;
    match pending.login(user, &password).await {
        Ok(()) => {}
        Err(ClientError::BadCredentials { retry_after }) if !retry_after.is_zero() => {
            bail!("wrong username or password; next attempt allowed in {} s", retry_after.as_secs().max(1))
        }
        Err(e) => return Err(e.into()),
    }
    let now =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    trust.trust(&endpoint.to_string(), &pending.fingerprint, &pending.host.device_name, now);
    store.save_trust(&trust)?;
    let mut session = pending.into_session();
    if !session.host_info.mac_addresses.is_empty() {
        println!("MAC: {}", session.host_info.mac_addresses.join(", "));
    }
    for _ in 0..pings {
        println!("ping: {:.2} ms", session.ping().await?.as_secs_f64() * 1000.0);
    }
    session.close("done").await;
    Ok(())
}
