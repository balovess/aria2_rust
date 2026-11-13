use super::*;
use tokio::net::TcpListener;

#[cfg(test)]
mod process_lifecycle;
#[cfg(test)]
mod route_lifecycle;

#[test]
fn zero_incoming_handshake_timeout_keeps_one_second_minimum() {
    let manager = BtPeerListenerManager::new();
    manager.set_incoming_handshake_timeout(std::time::Duration::ZERO);

    assert_eq!(
        manager
            .incoming_handshake_timeout_nanos
            .load(std::sync::atomic::Ordering::Relaxed),
        1_000_000_000
    );
}

#[tokio::test]
async fn configured_incoming_handshake_timeout_is_applied_by_shared_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    manager.set_incoming_handshake_timeout(std::time::Duration::from_millis(250));
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let (port, _incoming, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: [91; 20],
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 91,
            max_peers: 1,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream.write_all(b"x").await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(800),
        stream.read(&mut byte),
    )
    .await
    .expect("subsecond configured timeout should close the incomplete peer");
    assert_eq!(read.unwrap(), 0);
}

#[tokio::test]
async fn shared_listener_rejects_tcp_handshakes_when_global_slots_are_full() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut manager = BtPeerListenerManager::new();
    manager.incoming_handshake_slots = Arc::new(tokio::sync::Semaphore::new(1));
    manager.set_incoming_handshake_timeout(std::time::Duration::from_secs(5));
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let (port, _incoming, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: [92; 20],
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 92,
            max_peers: 2,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    let mut first = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    first.write_all(b"x").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut second = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    second.write_all(b"x").await.unwrap();
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        second.read(&mut byte),
    )
    .await
    .expect("over-capacity incoming peer should be closed promptly");
    assert!(matches!(read, Err(_) | Ok(0)));

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), first.read(&mut byte),)
            .await
            .is_err()
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn tcp_and_utp_listeners_share_the_handshake_slot_limit() {
    use tokio::io::AsyncWriteExt;

    let mut manager = BtPeerListenerManager::new();
    manager.incoming_handshake_slots = Arc::new(tokio::sync::Semaphore::new(1));
    manager.set_incoming_handshake_timeout(std::time::Duration::from_secs(5));
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let (port, _incoming, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: [93; 20],
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 93,
            max_peers: 2,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();
    let _utp_transport = manager
        .register_utp_transport("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();

    let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    tcp.write_all(b"x").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let endpoint = "127.0.0.1:6881".parse().unwrap();
    let (incoming, close_requested) =
        super::super::utp_transport::incoming_connection_for_test(endpoint);
    manager.utp_incoming_tx.send(incoming).await.unwrap();

    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        while !close_requested.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("uTP peer should be dropped while the TCP handshake holds the only slot");

    manager.shutdown().await;
}

#[tokio::test]
async fn shared_manager_routes_two_torrents_on_one_socket() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let storage_a = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let storage_b = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let hash_a = [11u8; 20];
    let hash_b = [22u8; 20];
    let (port, mut rx_a, route_a) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: hash_a,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 4,
            peer_storage: storage_a,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();
    let (_, mut rx_b, route_b) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![port],
            info_hash: hash_b,
            info_hash_v2: None,
            local_peer_id: [2; 20],
            caretaker_id: 2,
            max_peers: 4,
            peer_storage: storage_b,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    async fn connect_and_handshake(port: u16, hash: [u8; 20], peer_id: [u8; 20]) {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(&Handshake::new(&hash, &peer_id).to_bytes())
            .await
            .unwrap();
        let mut response = [0u8; 68];
        stream.read_exact(&mut response).await.unwrap();
        assert_eq!(Handshake::parse(&response).unwrap().info_hash, hash);
    }

    let first = tokio::spawn(connect_and_handshake(port, hash_a, [3; 20]));
    let incoming_a = tokio::time::timeout(std::time::Duration::from_secs(2), rx_a.recv())
        .await
        .unwrap()
        .unwrap();
    first.await.unwrap();
    assert_eq!(incoming_a.connection.remote_peer_id(), Some([3; 20]));

    let second = tokio::spawn(connect_and_handshake(port, hash_b, [4; 20]));
    let incoming_b = tokio::time::timeout(std::time::Duration::from_secs(2), rx_b.recv())
        .await
        .unwrap()
        .unwrap();
    second.await.unwrap();
    assert_eq!(incoming_b.connection.remote_peer_id(), Some([4; 20]));
    assert!(rx_a.try_recv().is_err());

    drop(route_a);
    drop(route_b);
}

#[tokio::test]
async fn route_dht_capabilities_update_after_registration() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let info_hash = [31; 20];
    let (port, mut incoming_peers, route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 31,
            max_peers: 1,
            peer_storage: Arc::new(Mutex::new(DefaultPeerStorage::new())),
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    route.set_dht_capabilities(true, false);
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(&Handshake::new(&info_hash, &[2; 20]).to_bytes())
        .await
        .unwrap();
    let mut response = [0u8; 68];
    stream.read_exact(&mut response).await.unwrap();
    assert!(Handshake::parse(&response).unwrap().supports_dht());
    incoming_peers.recv().await.unwrap();
}

#[tokio::test]
async fn ipv6_registration_also_accepts_ipv4_peers() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let info_hash = [55u8; 20];
    let (port, mut incoming_peers, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
            ports: vec![0],
            info_hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 55,
            max_peers: 1,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .expect("IPv6 listener should be available for this test");

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("an IPv6-enabled BT listener must accept IPv4 peers too");
    stream
        .write_all(&Handshake::new(&info_hash, &[2; 20]).to_bytes())
        .await
        .unwrap();
    let mut response = [0u8; 68];
    stream.read_exact(&mut response).await.unwrap();
    assert_eq!(Handshake::parse(&response).unwrap().info_hash, info_hash);

    let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), incoming_peers.recv())
        .await
        .expect("IPv4 peer admission timed out")
        .expect("IPv4 peer was not routed");
    assert_eq!(
        incoming.endpoint.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[tokio::test]
async fn shared_manager_unregisters_route_on_handle_drop() {
    let manager = BtPeerListenerManager::new();
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let hash = [33u8; 20];
    let (_, _, route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 1,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();
    drop(route);
    assert!(manager.routes.read().unwrap().get(&hash).is_none());
}

#[tokio::test]
async fn shared_manager_answers_hybrid_peer_with_v2_hash() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let v1 = [61u8; 20];
    let v2 = [62u8; 32];
    let (port, mut incoming_peers, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: v1,
            info_hash_v2: Some(v2),
            local_peer_id: [1; 20],
            caretaker_id: 61,
            max_peers: 1,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(&Handshake::new(&v1, &[9; 20]).with_bep52(true).to_bytes())
        .await
        .unwrap();
    let mut response = [0u8; 68];
    stream.read_exact(&mut response).await.unwrap();
    let response = Handshake::parse(&response).unwrap();
    assert_eq!(response.info_hash, v2[..20]);
    assert!(response.supports_bep52());
    let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), incoming_peers.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incoming.connection.remote_peer_id(), Some([9; 20]));
}

#[tokio::test]
async fn dropping_one_manager_clone_keeps_listener_alive_until_last_owner_drops() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let info_hash = [43u8; 20];
    let (port, mut incoming_peers, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 1,
            peer_storage: Arc::new(Mutex::new(DefaultPeerStorage::new())),
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();
    let remaining_owner = manager.clone();
    drop(manager);

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("shared listener socket should remain bound");
    stream
        .write_all(&Handshake::new(&info_hash, &[9; 20]).to_bytes())
        .await
        .unwrap();
    let mut response = [0u8; 68];
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_exact(&mut response),
    )
    .await
    .expect("dropping one manager clone must not stop peer acceptance")
    .unwrap();
    assert_eq!(Handshake::parse(&response).unwrap().info_hash, info_hash);
    let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), incoming_peers.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incoming.connection.remote_peer_id(), Some([9; 20]));

    drop(remaining_owner);
    let rebound = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await {
                break listener;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("dropping the last manager owner must release the listener socket");
    drop(rebound);
}
#[tokio::test]
async fn burst_of_aborted_incoming_connections_does_not_stop_peer_acceptance() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use socket2::{Domain, Protocol, Socket, Type};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let info_hash = [46u8; 20];
    let (port, mut incoming_peers, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 1,
            peer_storage: Arc::new(Mutex::new(DefaultPeerStorage::new())),
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    // Queue reset connections before yielding back to the accept task. Some
    // operating systems surface one as an accept error instead of a stream.
    for _ in 0..64 {
        let aborted = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
        aborted.set_linger(Some(std::time::Duration::ZERO)).unwrap();
        aborted
            .connect(&SocketAddr::from(([127, 0, 0, 1], port)).into())
            .unwrap();
        drop(aborted);
    }

    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("listener should remain available after aborted connections");
    stream
        .write_all(&Handshake::new(&info_hash, &[11; 20]).to_bytes())
        .await
        .unwrap();
    let mut response = [0u8; 68];
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        stream.read_exact(&mut response),
    )
    .await
    .expect("listener must accept peers after recoverable accept errors")
    .unwrap();
    assert_eq!(Handshake::parse(&response).unwrap().info_hash, info_hash);
    let incoming = tokio::time::timeout(std::time::Duration::from_secs(2), incoming_peers.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(incoming.connection.remote_peer_id(), Some([11; 20]));
}

#[tokio::test]
async fn shutdown_releases_the_shared_listener_socket() {
    let manager = BtPeerListenerManager::new();
    let storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let (port, _receiver, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: [44u8; 20],
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 1,
            peer_storage: storage,
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    manager.shutdown().await;
    let rebound = TcpListener::bind(("127.0.0.1", port))
        .await
        .expect("shutdown must wait for the TCP accept actor to release its socket");
    drop(rebound);

    let registration = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash: [45u8; 20],
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            max_peers: 1,
            peer_storage: Arc::new(Mutex::new(DefaultPeerStorage::new())),
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await;
    assert_eq!(
        registration
            .err()
            .expect("shutdown rejects new routes")
            .kind(),
        std::io::ErrorKind::NotConnected
    );
}
