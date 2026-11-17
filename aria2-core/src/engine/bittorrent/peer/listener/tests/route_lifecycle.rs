use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::super::{BtPeerListenerManager, BtPeerRouteConfig};
use crate::engine::bittorrent::peer::storage::DefaultPeerStorage;

#[tokio::test]
async fn dropping_route_cancels_blocked_peer_delivery_and_releases_admission() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let info_hash = [73; 20];
    let peer_storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let (port, mut incoming_peers, route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 0,
            peer_storage: Arc::clone(&peer_storage),
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    async fn connect_and_finish_handshake(
        port: u16,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
    ) -> tokio::net::TcpStream {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(&Handshake::new(&info_hash, &peer_id).to_bytes())
            .await
            .unwrap();
        let mut response = [0; 68];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(Handshake::parse(&response).unwrap().info_hash, info_hash);
        stream
    }

    let _queued_peer = connect_and_finish_handshake(port, info_hash, [2; 20]).await;
    let mut blocked_peer = connect_and_finish_handshake(port, info_hash, [3; 20]).await;

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if peer_storage.lock().unwrap().used_peers().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("second peer should reach admission before its bounded delivery blocks");

    drop(route);

    let mut byte = [0; 1];
    let read = tokio::time::timeout(Duration::from_millis(250), blocked_peer.read(&mut byte))
        .await
        .expect("route removal should cancel a peer blocked on mailbox delivery")
        .unwrap();
    assert_eq!(read, 0, "the blocked peer connection should be closed");
    assert!(incoming_peers.try_recv().is_ok());

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if peer_storage.lock().unwrap().used_peers().len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("cancelled route delivery should return the peer-storage reservation");

    manager.shutdown().await;
}
