use super::*;

#[tokio::test]
async fn shutdown_closes_an_inbound_peer_blocked_by_route_backpressure() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let manager = BtPeerListenerManager::new();
    let info_hash = [47u8; 20];
    let peer_storage = Arc::new(Mutex::new(DefaultPeerStorage::new()));
    let (port, mut incoming_peers, _route) = manager
        .register(BtPeerRouteConfig {
            bind_ip: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            ports: vec![0],
            info_hash,
            info_hash_v2: None,
            local_peer_id: [1; 20],
            caretaker_id: 1,
            // Zero means unlimited admission; the internal mailbox stays
            // bounded at one so the second peer blocks in route delivery.
            max_peers: 0,
            peer_storage: Arc::clone(&peer_storage),
            crypto_policy: Default::default(),
            dht_enabled: false,
        })
        .await
        .unwrap();

    async fn connect_and_finish_handshake(
        port: u16,
        hash: [u8; 20],
        peer_id: [u8; 20],
    ) -> tokio::net::TcpStream {
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
        stream
    }

    // The first completed handshake fills the route mailbox. The second
    // completes its wire handshake but remains blocked while delivering its
    // IncomingPeer, proving that shutdown also owns per-connection tasks.
    let _queued_peer = connect_and_finish_handshake(port, info_hash, [2; 20]).await;
    let mut blocked_peer = connect_and_finish_handshake(port, info_hash, [3; 20]).await;

    manager.shutdown().await;

    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(
        std::time::Duration::from_millis(250),
        blocked_peer.read(&mut byte),
    )
    .await
    .expect("shutdown must cancel a route-delivery task blocked by backpressure")
    .unwrap();
    assert_eq!(read, 0, "the pending inbound peer socket must be closed");
    assert!(incoming_peers.try_recv().is_ok());
    assert_eq!(
        peer_storage.lock().unwrap().used_peers().len(),
        1,
        "shutdown must return the peer whose route delivery was cancelled"
    );
}

#[tokio::test]
async fn shutdown_waits_for_process_utp_transport_to_release_udp_port() {
    let manager = BtPeerListenerManager::new();
    let reservation = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);

    let _transport = manager
        .register_utp_transport(address)
        .await
        .expect("process uTP transport should bind");

    manager.shutdown().await;

    let rebound = tokio::net::UdpSocket::bind(address)
        .await
        .expect("shutdown must wait for the process uTP actor to release its socket");
    drop(rebound);
}

#[tokio::test]
async fn configured_utp_connection_limit_reaches_shared_transport_socket() {
    let manager = BtPeerListenerManager::new();
    manager.set_utp_socket_options(
        aria2_protocol::bittorrent::utp::UtpSocketOptions::default().with_max_connections(1),
    );
    let reservation = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);

    let transport = manager
        .register_utp_transport(address)
        .await
        .expect("process uTP transport should bind with configured options");
    let remote = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let endpoint = remote.local_addr().unwrap();

    let _first = transport
        .connect(endpoint)
        .await
        .expect("first uTP connection should fit the configured limit");
    assert!(transport.connect(endpoint).await.is_err());

    manager.shutdown().await;
}
