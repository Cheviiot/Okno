use std::time::Duration;

use okno_net::{Error, Identity};
use okno_proto::envelope::Msg;
use okno_proto::{FileChunk, FileMsg, InputEvent, KeyEvent, Ping, file_msg, input_event};
use tokio::net::{TcpListener, TcpStream};

async fn pair() -> (okno_net::Connection, okno_net::Connection, Identity, Identity) {
    let host_id = Identity::generate();
    let client_id = Identity::generate();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let host_key = Identity::from_private(host_id.private_key()).unwrap();
    let accept = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        okno_net::accept(stream, &host_key).await.unwrap()
    });
    let client = okno_net::connect(TcpStream::connect(addr).await.unwrap(), &client_id).await.unwrap();
    (client, accept.await.unwrap(), client_id, host_id)
}

fn key(code: u32) -> Msg {
    Msg::Input(InputEvent { event: Some(input_event::Event::Key(KeyEvent { evdev_code: code, pressed: true })) })
}

#[tokio::test]
async fn both_sides_learn_peer_fingerprint() {
    let (client, host, client_id, host_id) = pair().await;
    assert_eq!(client.remote_fingerprint(), host_id.fingerprint());
    assert_eq!(host.remote_fingerprint(), client_id.fingerprint());
}

#[tokio::test]
async fn messages_round_trip_and_close() {
    let (client, mut host, _, _) = pair().await;
    let (tx, _rx) = client.into_parts();
    tx.send(Msg::Ping(Ping { nonce: 7 })).await.unwrap();
    assert_eq!(host.receiver().recv().await.unwrap(), Msg::Ping(Ping { nonce: 7 }));
    drop(tx);
    assert!(matches!(host.receiver().recv().await, Err(Error::Closed)));
}

#[tokio::test]
async fn input_overtakes_large_bulk_message() {
    let (client, mut host, _, _) = pair().await;
    let bulk = Msg::File(FileMsg {
        id: 1,
        op: Some(file_msg::Op::Chunk(FileChunk { offset: 0, data: vec![0xAB; 16 * 1024 * 1024] })),
    });
    client.sender().send(bulk.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    client.sender().send(key(30)).await.unwrap();

    let first = host.receiver().recv().await.unwrap();
    assert_eq!(first, key(30), "input must not wait behind the bulk message");
    assert_eq!(host.receiver().recv().await.unwrap(), bulk);
}

#[tokio::test]
async fn handshake_fails_when_peer_hangs_up() {
    // A handshake with a peer that disappears mid-way must fail, not hang.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        drop(stream);
    });
    let result = okno_net::connect(TcpStream::connect(addr).await.unwrap(), &Identity::generate()).await;
    assert!(result.is_err());
}
