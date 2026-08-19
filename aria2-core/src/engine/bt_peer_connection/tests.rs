//! Tests for the BitTorrent peer connection module.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use super::peer_conn::{BtPeerConn, KEEPALIVE_INTERVAL_SECS, PEER_TIMEOUT_SECS};
use super::session_resource::PeerSessionResource;
use super::types::SendBuffer;

// -----------------------------------------------------------------------
// SendBuffer tests
// -----------------------------------------------------------------------

#[test]
fn test_send_buffer_push_and_drain() {
    let mut buf = SendBuffer::new();
    assert!(buf.is_empty());
    assert_eq!(buf.len(), 0);

    buf.push_bytes(vec![1, 2, 3]);
    assert!(!buf.is_empty());
    assert_eq!(buf.len(), 3);

    buf.push_bytes(vec![4, 5, 6]);
    assert_eq!(buf.len(), 6);

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

    buf.clear();
    assert!(buf.is_empty());

    buf.push_bytes(vec![1]);
    let _ = buf.take_pending();
    assert!(buf.is_empty());
}

#[test]
fn test_send_buffer_encryption_flag() {
    let mut buf = SendBuffer::new();
    assert!(!buf.is_encryption_enabled());

    buf.set_encryption_enabled(true);
    assert!(buf.is_encryption_enabled());

    buf.set_encryption_enabled(false);
    assert!(!buf.is_encryption_enabled());
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
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);
    assert_eq!(res.bitfield_length, 1);

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
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert!(!res.is_seeder());

    res.mark_seeder();
    assert!(res.is_seeder());
    for i in 0..4 {
        assert!(res.has_piece(i), "seeder should have piece {}", i);
    }
}

#[test]
fn test_peer_session_resource_set_all_bitfield() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    res.set_all_bitfield();
    // 4 pieces in 1 byte = 0xF0 (upper 4 bits)
    assert_eq!(res.bitfield(), &[0xF0]);
    assert!(res.is_seeder());
}

#[test]
fn test_peer_session_resource_reconfigure() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert_eq!(res.num_pieces(), 4);

    res.reconfigure(512 * 1024, 4 * 1024 * 1024);
    assert_eq!(res.num_pieces(), 8);
    assert_eq!(res.bitfield_length, 1);
}

#[test]
fn test_peer_session_resource_out_of_range() {
    let res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert!(!res.has_piece(100)); // out of range
}

#[test]
fn test_peer_session_resource_update_bitfield_out_of_range() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    // Should not panic on out-of-range index
    res.update_bitfield(100, 1);
    assert!(!res.has_piece(100));
}

// -----------------------------------------------------------------------
// PeerSessionResource — Fast Extension tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_fast_extension() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert!(!res.is_fast_extension_enabled());

    res.set_fast_extension_enabled(true);
    assert!(res.is_fast_extension_enabled());

    // Peer-allowed index set
    res.add_peer_allowed_index(5);
    res.add_peer_allowed_index(10);
    assert!(res.is_in_peer_allowed_index_set(5));
    assert!(res.is_in_peer_allowed_index_set(10));
    assert!(!res.is_in_peer_allowed_index_set(7));

    // Am-allowed index set
    res.add_am_allowed_index(3);
    assert!(res.is_in_am_allowed_index_set(3));
    assert!(!res.is_in_am_allowed_index_set(5));
}

// -----------------------------------------------------------------------
// PeerSessionResource — Extension Protocol tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_extensions() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert!(!res.is_extended_messaging_enabled());

    res.set_extended_messaging_enabled(true);
    assert!(res.is_extended_messaging_enabled());

    // Register extensions
    res.add_extension("ut_pex", 1);
    res.add_extension("ut_metadata", 2);

    assert_eq!(res.get_extension_message_id("ut_pex"), Some(1));
    assert_eq!(res.get_extension_message_id("ut_metadata"), Some(2));
    assert_eq!(res.get_extension_message_id("unknown"), None);

    assert_eq!(res.get_extension_name(1), Some("ut_pex"));
    assert_eq!(res.get_extension_name(2), Some("ut_metadata"));
    assert_eq!(res.get_extension_name(99), None);
}

// -----------------------------------------------------------------------
// PeerSessionResource — DHT tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_dht() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert!(!res.is_dht_enabled());

    res.set_dht_enabled(true);
    assert!(res.is_dht_enabled());
}

// -----------------------------------------------------------------------
// PeerSessionResource — Choking tests
// -----------------------------------------------------------------------

#[test]
fn test_peer_session_resource_choking() {
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);

    // Default: choking_required = true, opt_unchoking = false
    assert!(res.choking_required());
    assert!(!res.opt_unchoking());
    assert!(!res.snubbing());
    assert!(res.should_be_choking());

    // Opt unchoking overrides choking requirement
    res.set_opt_unchoking(true);
    assert!(!res.should_be_choking());

    // Snubbing
    res.set_snubbing(true);
    assert!(res.snubbing());

    // Release choking requirement
    res.set_choking_required(false);
    assert!(!res.choking_required());
    assert!(!res.should_be_choking());
}

// -----------------------------------------------------------------------
// BtPeerConn — session resource lifecycle
// -----------------------------------------------------------------------

#[test]
fn test_bt_peer_conn_session_resource_lifecycle() {
    // We cannot easily construct a BtPeerConn without a real connection,
    // so test the resource management pattern directly.
    let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
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
async fn test_bt_peer_conn_sends_configured_peer_agent_on_wire() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (server, endpoint) = listener.accept().await.unwrap();
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
        server, [0u8; 20],
    );
    let mut connection = BtPeerConn::from_incoming_plain(peer, endpoint);

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
    let peer = aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
        server, [0u8; 20],
    );
    let mut connection = BtPeerConn::from_incoming_plain(peer, endpoint);
    connection.allocate_session_resource(16 * 1024, 16 * 1024);

    let mut handshake = aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new();
    handshake.with_ut_metadata(7).with_ut_pex(9);
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
    let mut res = PeerSessionResource::new(1024 * 1024, 100 * 1024 * 1024);
    assert_eq!(res.num_pieces(), 100);
    assert_eq!(res.bitfield_length, 13); // ceil(100/8) = 13

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
    let res = PeerSessionResource::new(0, 0);
    assert_eq!(res.num_pieces(), 0);
    // Vacuously a seeder
    assert!(res.is_seeder());
}

#[test]
fn test_peer_session_resource_count_outstanding_upload() {
    let res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert_eq!(res.count_outstanding_upload(), 0);
}

#[test]
fn test_peer_session_resource_accessors() {
    let res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
    assert_eq!(res.piece_length(), 256 * 1024);
    assert_eq!(res.total_length(), 1024 * 1024);
}
