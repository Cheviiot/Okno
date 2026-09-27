use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use okno_auth::{AllowList, Credentials, TrustStore};
use okno_core::desktop::DesktopHandler;
use okno_core::host::{Host, HostSettings};
use okno_core::remote::Remote;
#[cfg(unix)]
use okno_core::terminal::TerminalEvent;
use okno_core::{Endpoint, client};
use okno_desktop::TestDesktop;
use okno_net::Identity;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn session() -> (Host, Remote) {
    let settings = HostSettings {
        device_name: "desk".into(),
        port: 0,
        listen: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        allowed_networks: AllowList::default(),
        credentials: Credentials::new("admin", "hunter22").unwrap(),
        discoverable: false,
        services: vec![],
        approver: None,
    };
    let handler = DesktopHandler::new(Arc::new(TestDesktop::new(64, 64, 5))).with_audio(okno_audio::Source::Tone);
    let host = Host::start(Identity::generate(), settings, Arc::new(handler)).await.unwrap();
    let endpoint = Endpoint::from(host.local_addrs()[0]);
    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await.unwrap();
    pending.login("admin", "hunter22").await.unwrap();
    (host, pending.into_session().run(Arc::new(|_| {})))
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn terminal_runs_commands() {
    let (_host, remote) = session().await;
    let (terminal, mut events) = remote.terminals().open(80, 24);
    let mut output = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    // Type only after the shell prompt, as a person would: bash drops
    // input typed before readline starts.
    while !(output.trim_end().ends_with('$') || output.trim_end().ends_with('#') || output.trim_end().ends_with('>')) {
        match tokio::time::timeout_at(deadline, events.recv()).await.expect("prompt in time") {
            Some(TerminalEvent::Output(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("unexpected {other:?}"),
        }
    }
    terminal.input(b"echo okno-$((40+2))\r".to_vec());
    output.clear();
    while !output.contains("okno-42") {
        let event = tokio::time::timeout_at(deadline, events.recv()).await;
        let Ok(event) = event else { panic!("no okno-42 in time; output: {output:?}") };
        match event {
            Some(TerminalEvent::Output(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("unexpected {other:?}; output so far: {output}"),
        }
    }
    terminal.input(b"exit 7\r".to_vec());
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await.expect("exit in time") {
            Some(TerminalEvent::Exited(code)) => {
                assert_eq!(code, 7);
                break;
            }
            Some(TerminalEvent::Output(_)) => {}
            None => panic!("route closed without exit"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn port_forwarding_carries_data_both_ways() {
    let (_host, remote) = session().await;
    // An echo service "behind" the host.
    let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = echo.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = echo.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                while let Ok(n) = s.read(&mut buf).await {
                    if n == 0 || s.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let forward = remote.tunnels().forward("127.0.0.1:0".parse().unwrap(), target).await.unwrap();
    for round in 0..2 {
        let mut conn = tokio::net::TcpStream::connect(forward.local_addr()).await.unwrap();
        let payload = format!("ping {round}");
        conn.write_all(payload.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; payload.len()];
        tokio::time::timeout(Duration::from_secs(5), conn.read_exact(&mut buf)).await.expect("echo in time").unwrap();
        assert_eq!(buf, payload.as_bytes());
    }

    // An unreachable target closes the local connection.
    let dead = remote.tunnels().forward("127.0.0.1:0".parse().unwrap(), "127.0.0.1:1".into()).await.unwrap();
    let mut conn = tokio::net::TcpStream::connect(dead.local_addr()).await.unwrap();
    let mut buf = [0u8; 8];
    let n =
        tokio::time::timeout(Duration::from_secs(15), conn.read(&mut buf)).await.expect("closed in time").unwrap_or(0);
    assert_eq!(n, 0);
}

/// Output larger than the window arrives whole while the reader is slow.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn terminal_output_is_flow_controlled() {
    let (_host, remote) = session().await;
    let (terminal, mut events) = remote.terminals().open(80, 24);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut output = String::new();
    while !(output.trim_end().ends_with('$') || output.trim_end().ends_with('#') || output.trim_end().ends_with('>')) {
        match tokio::time::timeout_at(deadline, events.recv()).await.expect("prompt in time") {
            Some(TerminalEvent::Output(bytes)) => output.push_str(&String::from_utf8_lossy(&bytes)),
            other => panic!("unexpected {other:?}"),
        }
    }
    let size = 2 * okno_core::terminal::WINDOW + 999;
    terminal.input(format!("head -c {size} /dev/zero | tr '\\0' x; exit\r").into_bytes());
    let mut xs = 0;
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await.expect("output in time") {
            Some(TerminalEvent::Output(bytes)) => {
                xs += bytes.iter().filter(|&&b| b == b'x').count();
                tokio::time::sleep(Duration::from_micros(200)).await;
            }
            Some(TerminalEvent::Exited(_)) => break,
            None => panic!("route closed after {xs} bytes"),
        }
    }
    // The echoed command line contains one more 'x'.
    assert!(xs >= size, "{xs} < {size}");
}

/// More than the flow-control window one way, then a half-close: the reply
/// written after EOF must still arrive.
#[tokio::test(flavor = "multi_thread")]
async fn tunnel_keeps_flow_control_and_half_close() {
    let (_host, remote) = session().await;
    // Counts what it receives until EOF, then answers with the count.
    let counter = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = counter.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (mut s, _) = counter.accept().await.unwrap();
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match s.read(&mut buf).await.unwrap() {
                0 => break,
                n => total += n as u64,
            }
            // A slow reader, so the window fills up.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        s.write_all(&total.to_be_bytes()).await.unwrap();
    });
    let forward = remote.tunnels().forward("127.0.0.1:0".parse().unwrap(), target).await.unwrap();
    let mut conn = tokio::net::TcpStream::connect(forward.local_addr()).await.unwrap();
    let size = 3 * okno_core::tunnel::WINDOW + 12345;
    let data = vec![7u8; size];
    let (mut rd, mut wr) = conn.split();
    let send = async {
        wr.write_all(&data).await.unwrap();
        wr.shutdown().await.unwrap();
    };
    let mut reply = [0u8; 8];
    let receive = async { tokio::time::timeout(Duration::from_secs(20), rd.read_exact(&mut reply)).await };
    let ((), received) = tokio::join!(send, receive);
    received.expect("reply in time").unwrap();
    assert_eq!(u64::from_be_bytes(reply), size as u64);
}

#[tokio::test(flavor = "multi_thread")]
async fn sound_arrives_as_a_tone() {
    let (_host, remote) = session().await;
    let ring = Arc::new(okno_audio::SampleRing::new(0, 2000));
    remote.start_audio(ring.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let mut samples = vec![0f32; okno_audio::FRAME_SAMPLES * 10];
    assert!(ring.buffered() >= samples.len(), "only {} samples", ring.buffered());
    ring.pull(&mut samples);
    let power = samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32;
    // A 0.3 sine has mean power 0.045.
    assert!((0.02..0.07).contains(&power), "power {power}");
    remote.stop_audio();
}
