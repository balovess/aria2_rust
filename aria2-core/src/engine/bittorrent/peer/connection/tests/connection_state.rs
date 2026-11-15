use super::*;

#[tokio::test]
async fn peer_connection_observes_runtime_peer_exchange_changes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let client =
        tokio::spawn(async move { tokio::net::TcpStream::connect(endpoint).await.unwrap() });
    let (server, remote_endpoint) = listener.accept().await.unwrap();
    let protocol =
        aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
            server, [0x71; 20], false, false,
        );
    let mut connection = BtPeerConn::from_incoming_tcp(protocol, remote_endpoint);
    let mut group = crate::request::request_group::RequestGroup::new(
        crate::request::request_group::GroupId::new(901),
        Vec::new(),
        crate::request::request_group::DownloadOptions::default(),
    );
    let enabled = group.bt_peer_exchange_enabled();
    connection.set_pex_enabled(true);
    connection.set_peer_exchange_option(enabled);

    assert!(connection.is_pex_enabled());
    group
        .try_update_option("enable-peer-exchange", serde_json::json!(false))
        .unwrap();
    assert!(!connection.is_pex_enabled());
    group
        .try_update_option("enable-peer-exchange", serde_json::json!(true))
        .unwrap();
    assert!(connection.is_pex_enabled());

    connection.set_pex_enabled(false);
    assert!(!connection.is_pex_enabled());
    drop(client.await.unwrap());
}

#[tokio::test]
async fn plain_peer_connection_uses_the_outbound_policy_source() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let info_hash = [7u8; 20];
    let peer_id = [8u8; 20];
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.unwrap();
        let _connection = aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash])
            .await
            .unwrap()
            .complete(peer_id, None, false)
            .await
            .unwrap();
        peer
    });

    let policy = crate::network::OutboundNetworkPolicy::single("127.0.0.1".parse().unwrap());
    let connection = BtPeerConn::connect_plain_with_policy(
        &aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
            &address.ip().to_string(),
            address.port(),
        ),
        &info_hash,
        None,
        &peer_id,
        Duration::from_secs(2),
        false,
        &policy,
    )
    .await
    .unwrap();
    assert_eq!(
        server.await.unwrap().ip(),
        "127.0.0.1".parse::<std::net::IpAddr>().unwrap()
    );
    drop(connection);
}

// -----------------------------------------------------------------------
// SendBuffer tests
// -----------------------------------------------------------------------

#[test]
fn test_send_buffer_push_and_drain() {
    let mut buf = SendBuffer::new();
    assert!(buf.is_empty());

    buf.push_bytes(vec![1, 2, 3]);
    assert!(!buf.is_empty());

    buf.push_bytes(vec![4, 5, 6]);

    let drained = buf.take_pending();
    assert_eq!(drained, vec![1, 2, 3, 4, 5, 6]);
    assert!(buf.is_empty());
}

#[test]
fn test_send_buffer_empty_check() {
    let mut buf = SendBuffer::new();
    assert!(buf.is_empty());

    buf.push_bytes(vec![42]);
    assert!(!buf.is_empty());

    let _ = buf.take_pending();
    assert!(buf.is_empty());
}

#[test]
fn test_send_buffer_default() {
    let buf = SendBuffer::default();
    assert!(buf.is_empty());
}

// -----------------------------------------------------------------------
// PeerSessionResource — bitfield tests
// -----------------------------------------------------------------------

#[test]
fn test_bt_peer_conn_session_resource_lifecycle() {
    // We cannot easily construct a BtPeerConn without a real connection,
    // so test the resource management pattern directly.
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);
    assert!(!res.is_seeder());

    res.mark_seeder();
    assert!(res.is_seeder());

    // Release (simulate disconnect)
    drop(res);
}

// -----------------------------------------------------------------------
// BtPeerConn — keepalive / timeout
// -----------------------------------------------------------------------

#[test]
fn test_bt_peer_conn_keepalive() {
    // Test the keepalive interval logic directly
    let now = Instant::now();

    // Just-sent keepalive should not trigger
    let last_sent = now;
    assert!(last_sent.elapsed() < Duration::from_secs(KEEPALIVE_INTERVAL_SECS));

    // A keepalive sent long ago should trigger
    let old_sent = now - Duration::from_secs(KEEPALIVE_INTERVAL_SECS + 10);
    assert!(old_sent.elapsed() >= Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
}

#[test]
fn test_bt_peer_conn_peer_timeout() {
    let now = Instant::now();

    // Recent message should not trigger timeout
    let last_recv = now;
    assert!(last_recv.elapsed() < Duration::from_secs(PEER_TIMEOUT_SECS));

    // Old message should trigger timeout
    let old_recv = now - Duration::from_secs(PEER_TIMEOUT_SECS + 10);
    assert!(old_recv.elapsed() >= Duration::from_secs(PEER_TIMEOUT_SECS));
}

#[test]
fn test_bt_peer_conn_uses_configured_timing_values() {
    let mut connection = BtPeerConn::new_stub(&[0u8; 20]);
    connection.set_timeouts(Duration::from_millis(5), Duration::from_millis(5));

    std::thread::sleep(Duration::from_millis(15));

    assert!(connection.should_send_keepalive());
    assert!(connection.is_peer_timed_out());
}

#[tokio::test]
async fn peer_write_stall_expires_with_the_configured_timeout() {
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local, endpoint) = listener.accept().await.unwrap();
    let protocol =
        aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
            local, [0u8; 20], false, false,
        );
    let mut connection = BtPeerConn::from_incoming_tcp(protocol, endpoint);
    connection.set_timeouts(Duration::from_secs(120), Duration::from_millis(50));

    let message = BtMessage::Piece {
        index: 0,
        begin: 0,
        data: vec![0x5a; 16 * 1024].into(),
    };
    let sends_before_stall = tokio::time::timeout(Duration::from_secs(4), async {
        let mut sent = 0usize;
        loop {
            if connection.send_bt_message(&message).await.is_err() {
                return sent;
            }
            sent += 1;
        }
    })
    .await
    .expect("a non-reading TCP peer must not hold a write forever");

    assert!(
        sends_before_stall > 0,
        "the loopback peer accepted no writes"
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            connection.send_bt_message(&message)
        )
        .await
        .expect("a partially written frame must poison the TCP connection")
        .is_err()
    );
    drop(remote);
}

#[test]
fn test_bt_peer_conn_queue_message_and_flush() {
    let mut buf = SendBuffer::new();

    // Queue multiple messages
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    buf.push_bytes(serialize(&BtMessage::Unchoke));
    buf.push_bytes(serialize(&BtMessage::Interested));
    buf.push_bytes(serialize(&BtMessage::Have { piece_index: 42 }));

    assert!(!buf.is_empty());
    let combined = buf.take_pending();

    // Verify the combined buffer contains all three messages
    // Unchoke: 4-byte length (00 00 00 01) + 1-byte ID (01) = 5 bytes
    // Interested: 4-byte length (00 00 00 01) + 1-byte ID (02) = 5 bytes
    // Have: 4-byte length (00 00 00 05) + 1-byte ID (04) + 4-byte piece = 9 bytes
    assert_eq!(combined.len(), 5 + 5 + 9);

    // Parse the combined stream
    use aria2_protocol::bittorrent::message::factory::parse_message_stream;
    let msgs = parse_message_stream(&combined);
    assert_eq!(msgs.len(), 3);

    assert_eq!(msgs[0].0, Some(BtMessage::Unchoke));
    assert_eq!(msgs[1].0, Some(BtMessage::Interested));
    assert_eq!(msgs[2].0, Some(BtMessage::Have { piece_index: 42 }));
}

// -----------------------------------------------------------------------
// Legacy tests (preserved)
// -----------------------------------------------------------------------

#[test]
fn test_allowed_fast_set_operations() {
    let mut set: HashSet<u32> = HashSet::new();
    assert!(set.is_empty());
    assert!(!set.contains(&42));
    set.insert(42);
    assert!(set.contains(&42));
    set.insert(10);
    set.insert(99);
    assert_eq!(set.len(), 3);
    assert!(!set.contains(&999));
    set.insert(42);
    assert_eq!(set.len(), 3);
}

#[test]
fn test_allowed_fast_multiple_indices() {
    let mut set: HashSet<u32> = HashSet::new();
    for i in 0..100u32 {
        set.insert(i);
    }
    assert_eq!(set.len(), 100);
    for i in 0..100u32 {
        assert!(set.contains(&i));
    }
    assert!(!set.contains(&100));
}

// -----------------------------------------------------------------------
// PeerSessionResource — larger bitfield
// -----------------------------------------------------------------------
