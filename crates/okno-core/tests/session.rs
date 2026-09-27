use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use okno_auth::{AllowList, Credentials, TrustDecision, TrustStore};
use okno_core::Endpoint;
use okno_core::client::{self, ClientError};
use okno_core::host::{ControlOnly, Host, HostEvent, HostSettings};
use okno_net::Identity;

async fn start_host(allowed: AllowList) -> (Host, Endpoint, Identity) {
    let identity = Identity::generate();
    let copy = Identity::from_private(identity.private_key()).unwrap();
    let settings = HostSettings {
        device_name: "test-host".into(),
        port: 0,
        listen: vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        allowed_networks: allowed,
        credentials: Credentials::new("admin", "hunter22").unwrap(),
        discoverable: false,
        services: vec![],
    };
    let host = Host::start(identity, settings, Arc::new(ControlOnly)).await.unwrap();
    let endpoint = Endpoint::from(host.local_addrs()[0]);
    (host, endpoint, copy)
}

#[tokio::test]
async fn login_and_ping() {
    let (host, endpoint, host_id) = start_host(AllowList::default()).await;
    let mut events = host.subscribe();
    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "client").await.unwrap();
    assert_eq!(pending.host.device_name, "test-host");
    assert_eq!(pending.fingerprint, host_id.fingerprint());
    assert_eq!(pending.trust, TrustDecision::New);

    pending.login("admin", "hunter22").await.unwrap();
    let mut session = pending.into_session();
    assert!(session.ping().await.unwrap().as_secs() < 1);

    let opened = loop {
        if let HostEvent::SessionOpened(info) = events.recv().await.unwrap() {
            break info;
        }
    };
    assert_eq!(opened.device_name, "client");
    assert_eq!(opened.username, "admin");
    session.close("bye").await;
}

#[tokio::test]
async fn wrong_password_can_be_retried_then_throttles() {
    let (_host, endpoint, _) = start_host(AllowList::default()).await;
    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await.unwrap();
    for _ in 0..2 {
        let err = pending.login("admin", "wrong-one").await.unwrap_err();
        assert!(matches!(err, ClientError::BadCredentials { retry_after } if retry_after.is_zero()));
    }
    let err = pending.login("admin", "wrong-one").await.unwrap_err();
    assert!(matches!(err, ClientError::BadCredentials { retry_after } if !retry_after.is_zero()));
    // Even the right password is refused during the backoff.
    let err = pending.login("admin", "hunter22").await.unwrap_err();
    assert!(matches!(err, ClientError::Throttled { .. }), "{err:?}");
    assert!(!pending.is_logged_in());
}

#[tokio::test]
async fn changed_host_key_is_reported() {
    let (_host, endpoint, _) = start_host(AllowList::default()).await;
    let mut trust = TrustStore::default();
    // Pretend another key used to live at this endpoint.
    trust.trust(&endpoint.to_string(), &Identity::generate().fingerprint(), "old", 0);
    let pending = client::open(&endpoint, &Identity::generate(), &trust, "c").await.unwrap();
    assert!(matches!(pending.trust, TrustDecision::Changed { .. }));
}

#[tokio::test]
async fn host_refuses_networks_outside_allowlist() {
    let (host, endpoint, _) = start_host(AllowList::parse(["10.0.0.0/8"]).unwrap()).await;
    let mut events = host.subscribe();
    let result = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await;
    assert!(result.is_err());
    assert!(matches!(events.recv().await.unwrap(), HostEvent::Refused { .. }));
}

#[tokio::test]
async fn host_can_disconnect_and_change_password() {
    let (host, endpoint, _) = start_host(AllowList::default()).await;
    let mut events = host.subscribe();
    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await.unwrap();
    pending.login("admin", "hunter22").await.unwrap();
    let mut session = pending.into_session();
    let id = loop {
        if let HostEvent::SessionOpened(info) = events.recv().await.unwrap() {
            break info.id;
        }
    };
    assert!(host.disconnect(id));
    assert!(session.ping().await.is_err());

    host.set_credentials(Credentials::new("admin", "new-password").unwrap());
    let mut pending = client::open(&endpoint, &Identity::generate(), &TrustStore::default(), "c").await.unwrap();
    assert!(pending.login("admin", "hunter22").await.is_err());
    pending.login("admin", "new-password").await.unwrap();
}
