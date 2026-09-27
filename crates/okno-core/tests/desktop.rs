use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;

use okno_auth::{AllowList, Credentials, TrustStore};
use okno_core::Endpoint;
use okno_core::client;
use okno_core::desktop::DesktopHandler;
use okno_core::host::{Host, HostSettings};
use okno_core::remote::RemoteEvent;
use okno_desktop::TestDesktop;
use okno_net::Identity;
use okno_proto::{InputEvent, KeyEvent, input_event};
use tokio::sync::mpsc;

#[tokio::test(flavor = "multi_thread")]
async fn streams_video_and_injects_input() {
    let desktop = Arc::new(TestDesktop::new(320, 240, 30));
    let inputs = desktop.inputs();
    let pasted = desktop.pasted();
    let settings = HostSettings {
        device_name: "desk".into(),
        port: 0,
        listen: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        allowed_networks: AllowList::default(),
        credentials: Credentials::new("admin", "hunter22").unwrap(),
        discoverable: false,
        services: vec!["desktop".into()],
    };
    let host =
        Host::start(Identity::generate(), settings, Arc::new(DesktopHandler::new(desktop.clone()))).await.unwrap();
    let endpoint = Endpoint::from(host.local_addrs()[0]);

    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await.unwrap();
    pending.login("admin", "hunter22").await.unwrap();
    let session = pending.into_session();
    assert_eq!(session.host_info.displays.len(), 1);
    assert_eq!(session.host_info.displays[0].width, 320);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let remote = session.run(Arc::new(move |e| {
        let _ = tx.send(e);
    }));
    remote.start_video(0, 30, 2000).await.unwrap();

    let mut frames = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while frames < 10 {
        let event = tokio::time::timeout_at(deadline, rx.recv()).await.expect("frames arrive in time").unwrap();
        match event {
            RemoteEvent::Frame { width, height, rgba, .. } => {
                assert_eq!((width, height), (320, 240));
                assert_eq!(rgba.len(), 320 * 240 * 4);
                // The pattern's green channel grows with x: left edge dark,
                // right edge brighter.
                let left = rgba[4 * (120 * 320 + 2) + 1];
                let right = rgba[4 * (120 * 320 + 316) + 1];
                assert!(right > left + 60, "left {left} right {right}");
                frames += 1;
            }
            RemoteEvent::Error(e) => panic!("remote error: {e}"),
            _ => {}
        }
    }

    remote.send_input(InputEvent { event: Some(input_event::Event::Key(KeyEvent { evdev_code: 30, pressed: true })) });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(inputs.lock().unwrap().len(), 1);

    // Clipboard both ways.
    desktop.copy("copied on host");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await.expect("clipboard arrives").unwrap() {
            RemoteEvent::Clipboard(text) => {
                assert_eq!(text, "copied on host");
                break;
            }
            _ => continue,
        }
    }
    remote.send_clipboard("copied on client".into());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(pasted.lock().unwrap().as_slice(), ["copied on client"]);
    remote.close().await;
}
