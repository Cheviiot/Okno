use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use okno_auth::{AllowList, Credentials, TrustStore};
use okno_core::Endpoint;
use okno_core::client;
use okno_core::desktop::DesktopHandler;
use okno_core::host::{Host, HostSettings};
use okno_desktop::TestDesktop;
use okno_net::Identity;

#[tokio::test(flavor = "multi_thread")]
async fn upload_list_and_download() {
    let incoming = tempfile::tempdir().unwrap();
    let local = tempfile::tempdir().unwrap();
    let settings = HostSettings {
        device_name: "desk".into(),
        port: 0,
        listen: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        allowed_networks: AllowList::default(),
        credentials: Credentials::new("admin", "hunter22").unwrap(),
        discoverable: false,
        services: vec![],
    };
    let handler = DesktopHandler::new(Arc::new(TestDesktop::new(64, 64, 5))).with_incoming(incoming.path().into());
    let host = Host::start(Identity::generate(), settings, Arc::new(handler)).await.unwrap();
    let endpoint = Endpoint::from(host.local_addrs()[0]);
    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await.unwrap();
    pending.login("admin", "hunter22").await.unwrap();
    let remote = pending.into_session().run(Arc::new(|_| {}));
    let files = remote.files();

    // 1.5 MiB of varied bytes: several chunks.
    let data: Vec<u8> = (0..1_500_000u32).map(|i| (i * 31 % 251) as u8).collect();
    let source = local.path().join("photo.bin");
    std::fs::write(&source, &data).unwrap();

    let last = std::sync::Mutex::new((0, 0));
    files.upload(&source, |done, total| *last.lock().unwrap() = (done, total)).await.unwrap();
    assert_eq!(*last.lock().unwrap(), (1_500_000, 1_500_000));
    assert_eq!(std::fs::read(incoming.path().join("photo.bin")).unwrap(), data);

    // A second upload with the same name gets a new one.
    files.upload(&source, |_, _| {}).await.unwrap();
    assert!(incoming.path().join("photo (2).bin").exists());

    let listing = files.list(incoming.path().to_str().unwrap()).await.unwrap();
    let names: Vec<_> = listing.entries.iter().map(|e| e.name.clone()).collect();
    assert_eq!(names, ["photo (2).bin", "photo.bin"]);

    let remote_path = format!("{}{}photo.bin", listing.path, listing.separator);
    let target = local.path().join("back.bin");
    files.download(&remote_path, &target, |_, _| {}).await.unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), data);

    let missing = files.download("/definitely/not/here", &local.path().join("x"), |_, _| {}).await;
    assert!(missing.is_err());
    assert!(!local.path().join("x").exists());
    remote.close().await;
}
