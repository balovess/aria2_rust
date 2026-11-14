use super::*;

#[tokio::test]
async fn local_udp_tracker_fixture_supports_bep15_announce() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind local UDP tracker fixture");
    let address = socket.local_addr().expect("local UDP tracker address");
    let server = tokio::spawn(async move {
        let mut request = [0u8; 256];
        let (length, peer) = socket
            .recv_from(&mut request)
            .await
            .expect("receive BEP 15 connect request");
        assert_eq!(length, 16);
        assert_eq!(&request[0..8], &0x41727101980u64.to_be_bytes());
        assert_eq!(&request[8..12], &0i32.to_be_bytes());
        let transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
        let connection_id = 0x0102_0304_0506_0708u64;
        let mut connect_response = Vec::with_capacity(16);
        connect_response.extend_from_slice(&0i32.to_be_bytes());
        connect_response.extend_from_slice(&transaction.to_be_bytes());
        connect_response.extend_from_slice(&connection_id.to_be_bytes());
        socket
            .send_to(&connect_response, peer)
            .await
            .expect("send BEP 15 connect response");

        let (length, peer) = socket
            .recv_from(&mut request)
            .await
            .expect("receive BEP 15 announce request");
        assert!(length >= 98);
        assert_eq!(&request[0..8], &connection_id.to_be_bytes());
        assert_eq!(&request[8..12], &1i32.to_be_bytes());
        assert_eq!(&request[96..98], &51413u16.to_be_bytes());
        let transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
        let mut announce_response = Vec::with_capacity(26);
        announce_response.extend_from_slice(&1i32.to_be_bytes());
        announce_response.extend_from_slice(&transaction.to_be_bytes());
        announce_response.extend_from_slice(&60u32.to_be_bytes());
        announce_response.extend_from_slice(&3u32.to_be_bytes());
        announce_response.extend_from_slice(&7u32.to_be_bytes());
        announce_response.extend_from_slice(&[192, 0, 2, 11, 0x1A, 0xE1]);
        socket
            .send_to(&announce_response, peer)
            .await
            .expect("send BEP 15 announce response");
    });

    let mut announcer = TrackerAnnouncer::new(&[vec![format!("udp://{address}/announce")]], &None);
    announcer.set_tcp_port(51413);
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));
    let result = announcer
        .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await
        .expect("UDP tracker fixture should return an announce result");

    assert_eq!(result.peers, vec![("192.0.2.11".to_string(), 6881)]);
    assert_eq!(result.interval, Duration::from_secs(60));
    assert_eq!(result.seeders, Some(7));
    assert_eq!(result.leechers, Some(3));
    server.await.expect("UDP tracker fixture should exit");
}

#[tokio::test]
async fn cancelling_udp_announce_discards_its_transaction_before_stopped_event() {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind local UDP tracker fixture");
    let address = socket.local_addr().expect("local UDP tracker address");
    let first_connect_seen = Arc::new(tokio::sync::Notify::new());
    let server_connect_seen = Arc::clone(&first_connect_seen);
    let server = tokio::spawn(async move {
        let mut request = [0u8; 256];
        let (length, _) = socket
            .recv_from(&mut request)
            .await
            .expect("receive initial CONNECT");
        assert_eq!(length, 16);
        assert_eq!(&request[8..12], &0i32.to_be_bytes());
        server_connect_seen.notify_one();

        let (length, peer) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut request))
                .await
                .expect("stopped announce should start a fresh CONNECT")
                .expect("receive replacement CONNECT");
        assert_eq!(length, 16);
        assert_eq!(&request[8..12], &0i32.to_be_bytes());
        let connect_transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
        let connection_id = 0x0102_0304_0506_0708u64;
        let mut response = Vec::with_capacity(16);
        response.extend_from_slice(&0i32.to_be_bytes());
        response.extend_from_slice(&connect_transaction.to_be_bytes());
        response.extend_from_slice(&connection_id.to_be_bytes());
        socket
            .send_to(&response, peer)
            .await
            .expect("respond to replacement CONNECT");

        let (length, peer) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut request))
                .await
                .expect("stopped announce should send an ANNOUNCE")
                .expect("receive stopped ANNOUNCE");
        assert!(length >= 98);
        assert_eq!(&request[0..8], &connection_id.to_be_bytes());
        assert_eq!(&request[8..12], &1i32.to_be_bytes());
        assert_eq!(
            u32::from_be_bytes(request[80..84].try_into().unwrap()),
            aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpEvent::Stopped as u32,
            "the stale pre-cancellation announce must not be sent in place of stopped"
        );
        let announce_transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
        let mut response = Vec::with_capacity(26);
        response.extend_from_slice(&1i32.to_be_bytes());
        response.extend_from_slice(&announce_transaction.to_be_bytes());
        response.extend_from_slice(&60u32.to_be_bytes());
        response.extend_from_slice(&0u32.to_be_bytes());
        response.extend_from_slice(&1u32.to_be_bytes());
        response.extend_from_slice(&[192, 0, 2, 11, 0x1A, 0xE1]);
        socket
            .send_to(&response, peer)
            .await
            .expect("respond to stopped ANNOUNCE");
    });

    let mut announcer = TrackerAnnouncer::new(&[vec![format!("udp://{address}/announce")]], &None);
    announcer.set_timeouts(Duration::from_secs(5), Duration::from_secs(1));
    announcer.set_stopped_timeout(Duration::from_secs(2));
    announcer
        .bt_announce_mut()
        .announce_list_mut()
        .set_event(AnnounceEvent::Downloading);

    let mut initial_announce = Box::pin(announcer.announce(&[0; 20], &[1; 20], 0, 1, 0));
    tokio::select! {
        _ = first_connect_seen.notified() => {}
        result = &mut initial_announce => panic!("initial UDP announce completed unexpectedly: {result:?}"),
    }
    drop(initial_announce);
    assert!(announcer.cancel_pending_announce());

    announcer
        .announce_stopped(&[0; 20], &[1; 20], 0, 1, 0)
        .await;
    assert!(announcer.stopped_sent);
    server.await.expect("UDP cancellation fixture should exit");
}

#[tokio::test]
async fn dual_stack_policy_uses_ipv6_source_for_ipv6_udp_tracker() {
    let socket = tokio::net::UdpSocket::bind("[::1]:0")
        .await
        .expect("bind local IPv6 UDP tracker fixture");
    let address = socket.local_addr().expect("local IPv6 tracker address");
    let server = tokio::spawn(async move {
        let mut request = [0u8; 256];
        let (length, peer) = socket
            .recv_from(&mut request)
            .await
            .expect("receive IPv6 BEP 15 connect request");
        assert_eq!(peer.ip(), "::1".parse::<std::net::IpAddr>().unwrap());
        assert_eq!(length, 16);
        assert_eq!(&request[0..8], &0x41727101980u64.to_be_bytes());
        assert_eq!(&request[8..12], &0i32.to_be_bytes());
        let transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
        let connection_id = 0x0102_0304_0506_0708u64;
        let mut connect_response = Vec::with_capacity(16);
        connect_response.extend_from_slice(&0i32.to_be_bytes());
        connect_response.extend_from_slice(&transaction.to_be_bytes());
        connect_response.extend_from_slice(&connection_id.to_be_bytes());
        socket
            .send_to(&connect_response, peer)
            .await
            .expect("send IPv6 BEP 15 connect response");

        let (length, peer) = socket
            .recv_from(&mut request)
            .await
            .expect("receive IPv6 BEP 15 announce request");
        assert!(length >= 98);
        assert_eq!(&request[0..8], &connection_id.to_be_bytes());
        assert_eq!(&request[8..12], &1i32.to_be_bytes());
        let transaction = u32::from_be_bytes(request[12..16].try_into().unwrap());
        let mut announce_response = Vec::with_capacity(26);
        announce_response.extend_from_slice(&1i32.to_be_bytes());
        announce_response.extend_from_slice(&transaction.to_be_bytes());
        announce_response.extend_from_slice(&60u32.to_be_bytes());
        announce_response.extend_from_slice(&3u32.to_be_bytes());
        announce_response.extend_from_slice(&7u32.to_be_bytes());
        announce_response.extend_from_slice(&[192, 0, 2, 11, 0x1A, 0xE1]);
        socket
            .send_to(&announce_response, peer)
            .await
            .expect("send IPv6 BEP 15 announce response");
    });

    let policy = Arc::new(
        OutboundNetworkPolicy::new(vec!["127.0.0.2".parse().unwrap(), "::1".parse().unwrap()])
            .expect("dual-stack policy should build"),
    );
    let mut announcer = TrackerAnnouncer::new(&[vec![format!("udp://{address}/announce")]], &None);
    announcer.set_outbound_network_policy(policy);
    announcer.set_timeouts(Duration::from_secs(2), Duration::from_secs(2));
    let result = announcer
        .announce(&[0u8; 20], &[1u8; 20], 0, 1, 0)
        .await
        .unwrap_or_else(|| {
            panic!(
                "IPv6 UDP tracker fixture announce failed: {:?}",
                announcer.last_failure_kind
            )
        });

    assert_eq!(result.peers, vec![("192.0.2.11".to_string(), 6881)]);
    server.await.expect("IPv6 UDP tracker fixture should exit");
}
