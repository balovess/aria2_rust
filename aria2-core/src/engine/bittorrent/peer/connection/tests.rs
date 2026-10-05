//! Tests for the BitTorrent peer connection module.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use super::peer_conn::{BtPeerConn, KEEPALIVE_INTERVAL_SECS, PEER_TIMEOUT_SECS};
use super::session_resource::PeerSessionResource;
use super::types::SendBuffer;

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
fn test_peer_session_resource_bitfield() {
    // 4 pieces of 256 KiB each = 1 MiB total
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);
    assert_eq!(res.bitfield_length(), 1);

    // Initially no pieces
    for i in 0..4 {
        assert!(!res.has_piece(i), "piece {} should not be set", i);
    }

    // Set piece 0
    res.update_bitfield(0, 1);
    assert!(res.has_piece(0));
    assert!(!res.has_piece(1));

    // Set piece 3
    res.update_bitfield(3, 1);
    assert!(res.has_piece(3));

    // Clear piece 0
    res.update_bitfield(0, 0);
    assert!(!res.has_piece(0));

    // Set bitfield from raw bytes
    res.set_bitfield(&[0xC0]); // bits 0 and 1
    assert!(res.has_piece(0));
    assert!(res.has_piece(1));
    assert!(!res.has_piece(2));
    assert!(!res.has_piece(3));
}

#[test]
fn test_peer_session_resource_seeder() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert!(!res.is_seeder());

    res.mark_seeder();
    assert!(res.is_seeder());
    for i in 0..4 {
        assert!(res.has_piece(i), "seeder should have piece {}", i);
    }
}

#[test]
fn test_peer_session_resource_reconfigure() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);

    res.reconfigure(512 * 1024, 8, 4 * 1024 * 1024);
    assert_eq!(res.num_pieces(), 8);
    assert_eq!(res.bitfield_length(), 1);
}

#[test]
fn test_peer_session_resource_out_of_range() {
    let res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert!(!res.has_piece(100)); // out of range
}

#[test]
fn test_peer_session_resource_update_bitfield_out_of_range() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    // Should not panic on out-of-range index
    res.update_bitfield(100, 1);
    assert!(!res.has_piece(100));
}

// -----------------------------------------------------------------------
// PeerSessionResource — Fast Extension tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_fast_extension() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert!(!res.is_fast_extension_enabled());

    res.set_fast_extension_enabled(true);
    assert!(res.is_fast_extension_enabled());
}

// -----------------------------------------------------------------------
// PeerSessionResource — Extension Protocol tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_extensions() {
    let mut res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    // Register extensions
    res.add_extension("ut_pex", 1);
    res.add_extension("ut_metadata", 2);

    assert_eq!(res.get_extension_message_id("ut_pex"), Some(1));
    assert_eq!(res.get_extension_message_id("ut_metadata"), Some(2));
    assert_eq!(res.get_extension_message_id("unknown"), None);
}

// -----------------------------------------------------------------------
// BtPeerConn — session resource lifecycle
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

#[tokio::test]
async fn test_bt_peer_conn_sends_configured_peer_agent_on_wire() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);

    connection
        .send_extension_handshake("contract-agent/1")
        .await
        .unwrap();

    let mut frame_length = [0u8; 4];
    tokio::io::AsyncReadExt::read_exact(&mut client, &mut frame_length)
        .await
        .unwrap();
    let payload_length = u32::from_be_bytes(frame_length) as usize;
    let mut frame = Vec::with_capacity(4 + payload_length);
    frame.extend_from_slice(&frame_length);
    let mut payload = vec![0u8; payload_length];
    tokio::io::AsyncReadExt::read_exact(&mut client, &mut payload)
        .await
        .unwrap();
    frame.extend_from_slice(&payload);

    let message = aria2_protocol::bittorrent::message::factory::parse_message(&frame).unwrap();
    match message {
        Some(aria2_protocol::bittorrent::message::types::BtMessage::Extended {
            ext_id,
            payload,
        }) => {
            assert_eq!(ext_id, 0);
            let handshake =
                aria2_protocol::bittorrent::message::extension::ExtensionHandshake::from_bytes(
                    &payload,
                )
                .unwrap();
            assert_eq!(handshake.v(), Some("contract-agent/1"));
        }
        other => panic!("expected extension handshake, got {other:?}"),
    }
}

#[tokio::test]
async fn test_bt_peer_conn_registers_remote_extension_ids() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    let mut handshake = aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new();
    handshake
        .with_version("remote-agent/2.3")
        .with_ut_metadata(7)
        .with_ut_pex(9);
    let frame = aria2_protocol::bittorrent::message::serializer::serialize(
        &aria2_protocol::bittorrent::message::types::BtMessage::Extended {
            ext_id: 0,
            payload: handshake.to_bytes(),
        },
    );
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .unwrap();

    assert!(connection.read_message().await.unwrap().is_some());
    assert_eq!(connection.peer_extension_id("ut_metadata"), Some(7));
    assert_eq!(connection.peer_extension_id("ut_pex"), Some(9));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string())
    );
}

#[tokio::test]
async fn bt_peer_conn_applies_incremental_extension_handshakes_and_disables_zero_ids() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    fn extension_handshake(extensions: &[(&[u8], u8)], client: Option<&str>) -> Vec<u8> {
        let mut m = BTreeMap::new();
        for (name, id) in extensions {
            m.insert(name.to_vec(), BencodeValue::Int(i64::from(*id)));
        }
        let mut root = BTreeMap::new();
        root.insert(b"m".to_vec(), BencodeValue::Dict(m));
        if let Some(client) = client {
            root.insert(
                b"v".to_vec(),
                BencodeValue::Bytes(client.as_bytes().to_vec()),
            );
        }
        BencodeValue::Dict(root).encode()
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    for (extensions, client_version) in [
        (
            vec![(b"ut_metadata".as_slice(), 7), (b"ut_pex".as_slice(), 19)],
            Some("remote-agent/2.3"),
        ),
        (vec![(b"ut_metadata".as_slice(), 8)], None),
        (vec![(b"ut_pex".as_slice(), 0)], None),
    ] {
        let frame = serialize(&BtMessage::Extended {
            ext_id: 0,
            payload: extension_handshake(&extensions, client_version),
        });
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .unwrap();
        assert!(connection.read_message().await.unwrap().is_some());
    }

    assert_eq!(connection.peer_extension_id("ut_metadata"), Some(8));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string()),
        "partial BEP 10 updates without v preserve the learned client name"
    );
    assert_eq!(
        connection.peer_extension_id("ut_pex"),
        None,
        "BEP 10 extension ID 0 disables that extension"
    );
}

#[tokio::test]
async fn bt_peer_conn_accepts_extension_handshake_without_m_dictionary() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    let payload = BencodeValue::Dict(BTreeMap::from([(
        b"v".to_vec(),
        BencodeValue::Bytes(b"remote-agent/2.3".to_vec()),
    )]))
    .encode();
    let frame = serialize(&BtMessage::Extended { ext_id: 0, payload });
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .unwrap();

    assert!(matches!(
        connection.read_message().await.unwrap(),
        Some(BtMessage::Extended { ext_id: 0, .. })
    ));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string()),
        "the optional m dictionary must not suppress the other handshake fields"
    );
    assert_eq!(connection.peer_extension_id("ut_metadata"), None);
    assert_eq!(connection.peer_extension_id("ut_pex"), None);
}

#[tokio::test]
async fn bt_peer_conn_keeps_other_fields_when_extension_map_has_wrong_type() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer_capabilities(
        server, [0u8; 20], false, false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    let payload = BencodeValue::Dict(BTreeMap::from([
        (b"m".to_vec(), BencodeValue::Int(42)),
        (b"p".to_vec(), BencodeValue::Int(6881)),
        (
            b"v".to_vec(),
            BencodeValue::Bytes(b"remote-agent/2.3".to_vec()),
        ),
    ]))
    .encode();
    let frame = serialize(&BtMessage::Extended { ext_id: 0, payload });
    tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
        .await
        .unwrap();

    assert!(matches!(
        connection.read_message().await.unwrap(),
        Some(BtMessage::Extended { ext_id: 0, .. })
    ));
    assert_eq!(connection.remote_listen_port, Some(6881));
    assert_eq!(
        *connection
            .remote_client
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        Some("remote-agent/2.3".to_string()),
        "an invalid optional m field must not suppress valid BEP 10 fields"
    );
    assert_eq!(connection.peer_extension_id("ut_metadata"), None);
    assert_eq!(connection.peer_extension_id("ut_pex"), None);
}

#[tokio::test]
async fn bt_peer_conn_enforces_extended_messaging_handshake_capability() {
    use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::message::types::BtMessage;

    let info_hash = [0x42u8; 20];
    for extended_messaging in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = tokio::spawn(async move {
            let (stream, endpoint) = listener.accept().await.unwrap();
            let incoming =
                aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash])
                    .await
                    .unwrap();
            let peer = incoming.complete([0x24u8; 20], None, false).await.unwrap();
            let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);
            connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);
            connection
        });

        let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
        let mut handshake = Handshake::new(&info_hash, &[0x23u8; 20]);
        handshake.reserved[5] &= !0x10;
        if extended_messaging {
            handshake.reserved[5] |= 0x10;
        }
        tokio::io::AsyncWriteExt::write_all(&mut client, &handshake.to_bytes())
            .await
            .unwrap();
        let mut response = [0u8; 68];
        tokio::io::AsyncReadExt::read_exact(&mut client, &mut response)
            .await
            .unwrap();
        assert!(
            Handshake::parse(&response)
                .unwrap()
                .supports_extended_messaging()
        );

        let mut connection = incoming.await.unwrap();
        let mut extension_handshake = ExtensionHandshake::new();
        extension_handshake.with_ut_pex(19);
        let frame = serialize(&BtMessage::Extended {
            ext_id: 0,
            payload: extension_handshake.to_bytes(),
        });
        tokio::io::AsyncWriteExt::write_all(&mut client, &frame)
            .await
            .unwrap();

        if extended_messaging {
            assert!(connection.read_message().await.unwrap().is_some());
            assert_eq!(connection.peer_extension_id("ut_pex"), Some(19));
        } else {
            let error = connection.read_message().await.unwrap_err().to_string();
            assert!(error.contains("extended message without negotiating extended messaging"));
            assert_eq!(connection.peer_extension_id("ut_pex"), None);
        }
    }
}

#[tokio::test]
async fn test_bt_peer_conn_initializes_fast_extension_from_handshake() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
        server, [0u8; 20], false, true,
    );
    let mut connection = BtPeerConn::from_incoming_tcp(peer, endpoint);

    connection.allocate_session_resource(16 * 1024, 1, 16 * 1024);

    assert!(connection.is_fast_extension_enabled());
    connection.add_peer_allowed_fast(0);
    connection.add_am_allowed_fast(1);
    assert!(connection.peer_allowed_fast_set().contains(&0));
    assert!(!connection.peer_allowed_fast_set().contains(&1));
    assert!(connection.allowed_fast_set().contains(&0));
    assert!(connection.am_allowed_fast(1));
    assert!(!connection.am_allowed_fast(0));
}

// -----------------------------------------------------------------------
// BtPeerConn — queue_message and flush (unit test of buffer logic)
// -----------------------------------------------------------------------

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

#[test]
fn test_peer_session_resource_large_bitfield() {
    // 100 pieces of 1 MiB each = 100 MiB total
    let mut res = PeerSessionResource::new(1024 * 1024, 100, 100 * 1024 * 1024);
    assert_eq!(res.num_pieces(), 100);
    assert_eq!(res.bitfield_length(), 13); // ceil(100/8) = 13

    // Set piece 0 and 99
    res.update_bitfield(0, 1);
    res.update_bitfield(99, 1);
    assert!(res.has_piece(0));
    assert!(res.has_piece(99));
    assert!(!res.has_piece(50));

    // Mark seeder — all 100 bits should be set
    res.mark_seeder();
    assert!(res.is_seeder());
    for i in 0..100 {
        assert!(res.has_piece(i), "seeder should have piece {}", i);
    }
    // Piece 100 is out of range
    assert!(!res.has_piece(100));
}

#[test]
fn test_peer_session_resource_zero_length() {
    let res = PeerSessionResource::new(0, 0, 0);
    assert_eq!(res.num_pieces(), 0);
    // Vacuously a seeder
    assert!(res.is_seeder());
}

#[test]
fn test_peer_session_resource_uses_explicit_piece_count() {
    // v2 multi-file content can be two bytes while occupying two aligned
    // protocol pieces.
    let res = PeerSessionResource::new(16 * 1024, 2, 2);
    assert_eq!(res.num_pieces(), 2);
    assert_eq!(res.bitfield_length(), 1);
}

#[test]
fn test_peer_session_resource_accessors() {
    let res = PeerSessionResource::new(256 * 1024, 4, 1024 * 1024);
    assert_eq!(res.piece_length(), 256 * 1024);
    assert_eq!(res.total_length(), 1024 * 1024);
}
