#![cfg(feature = "bittorrent")]

mod fixtures;
use aria2_core::engine::bittorrent::magnet::download_command::MagnetDownloadCommand;
use aria2_core::engine::bittorrent::magnet::metadata_collector::MetadataCollector;
use aria2_core::engine::bittorrent::magnet::metadata_exchange::{
    MetadataExchangeConfig, MetadataExchangeError, MetadataExchangeSession,
};
use aria2_core::engine::command::{Command, CommandStatus};
use aria2_core::network::OutboundNetworkPolicy;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_protocol::bittorrent::dht::compact::extract_compact_peers_from_response;
use aria2_protocol::bittorrent::magnet::MagnetLink;
use aria2_protocol::bittorrent::message::extension::{ExtensionHandshake, UtMetadataMessage};
use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use fixtures::mock_bt_peer::MockBtPeerServer;
use fixtures::mock_udp_tracker::MockUdpTracker;
use fixtures::test_torrent_builder::build_test_torrent;
use std::net::SocketAddr;
use tracing::info;

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn test_magnet_parse_hex_hash() {
    let magnet = "magnet:?xt=urn:btih:3b245e04703a1ec5c91cef3f2295ee88ab63c50d&dn=Ubuntu+22.04&tr=udp://tracker.example.com:1337/announce";
    let ml = MagnetLink::parse(magnet).unwrap();
    assert_eq!(
        ml.info_hash_hex(),
        "3b245e04703a1ec5c91cef3f2295ee88ab63c50d"
    );
    assert_eq!(ml.display_name, Some("Ubuntu 22.04".to_string()));
    assert_eq!(ml.trackers.len(), 1);
}

#[test]
fn test_magnet_parse_minimal() {
    let magnet = "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678";
    let ml = MagnetLink::parse(magnet).unwrap();
    assert!(ml.display_name.is_none());
    assert!(ml.trackers.is_empty());
}

#[test]
fn test_magnet_parse_invalid() {
    assert!(MagnetLink::parse("http://example.com").is_err());
    assert!(MagnetLink::parse("magnet:?dn=test").is_err());
}

#[test]
fn test_extension_handshake_roundtrip() {
    let mut hs = ExtensionHandshake::new();
    hs.with_metadata_size(12345);
    let encoded = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&encoded).unwrap();
    assert_eq!(parsed.metadata_size(), Some(12345));
    assert_eq!(parsed.ut_metadata_id(), Some(1));
}

#[test]
fn test_ut_metadata_request_encode_decode() {
    let msg = UtMetadataMessage::Request { piece: 0 };
    let decoded = UtMetadataMessage::from_payload(&msg.to_payload()).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn test_ut_metadata_data_encode_decode() {
    let data = b"fake torrent metadata".to_vec();
    let msg = UtMetadataMessage::Data {
        piece: 0,
        total_size: data.len() as u32,
        data,
    };
    let decoded = UtMetadataMessage::from_payload(&msg.to_payload()).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn test_ut_metadata_reject_encode_decode() {
    let msg = UtMetadataMessage::Reject { piece: 0 };
    let decoded = UtMetadataMessage::from_payload(&msg.to_payload()).unwrap();
    assert_eq!(decoded, msg);
}

#[test]
fn test_metadata_collector_basic() {
    let mut collector = MetadataCollector::new(2000, 1000).unwrap();
    assert!(!collector.is_complete());

    collector.add_piece(0, &vec![0xAB; 1000]);
    assert!(!collector.is_complete());
    assert!((collector.progress() - 0.5).abs() < 0.01);

    collector.add_piece(1, &vec![0xCD; 1000]);
    assert!(collector.is_complete());

    let assembled = collector.into_bytes().unwrap();
    assert_eq!(assembled.len(), 2000);
}

#[test]
fn test_metadata_collector_single_piece() {
    let mut collector = MetadataCollector::new(500, 16384).unwrap();

    collector.add_piece(0, &vec![0x42; 500]);
    assert!(collector.is_complete());

    let assembled = collector.into_bytes().unwrap();
    assert_eq!(assembled.len(), 500);
    assert!(assembled.iter().all(|&b| b == 0x42));
}

#[tokio::test]
async fn test_e2e_magnet_download_command_creation() {
    let dir = tmp_dir();
    let magnet_uri = "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=test_file";
    let options = DownloadOptions {
        timeout: Some(30),
        ..DownloadOptions::default()
    };

    let cmd =
        MagnetDownloadCommand::new(GroupId::new(99), magnet_uri, &options, dir.path().to_str());

    assert!(cmd.is_ok());
    let cmd = cmd.unwrap();

    assert!(matches!(cmd.status(), CommandStatus::Pending));
    assert_eq!(cmd.timeout(), Some(std::time::Duration::from_secs(30)));
}

#[tokio::test]
async fn test_e2e_magnet_download_invalid_input() {
    let result = MagnetDownloadCommand::new(
        GroupId::new(98),
        "not-a-magnet",
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err());
}

#[test]
fn test_magnet_url_decode() {
    assert_eq!(MagnetLink::url_decode("Hello%20World"), "Hello World");
    assert_eq!(
        MagnetLink::url_decode("name+with+spaces"),
        "name with spaces"
    );
}

#[tokio::test]
async fn test_metadata_exchange_no_peers_error() {
    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        connect_timeout: std::time::Duration::from_millis(50),
        request_timeout: std::time::Duration::from_millis(50),
        ..MetadataExchangeConfig::default()
    });
    let target_hash = [0x42u8; 20];
    let empty_peers: Vec<std::net::SocketAddr> = vec![];

    let result = session.fetch_metadata(&target_hash, &empty_peers).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        aria2_core::engine::bittorrent::magnet::metadata_exchange::MetadataExchangeError::NoPeersAvailable => {}
        other => panic!("Expected NoPeersAvailable, got {:?}", other),
    }
}

#[tokio::test]
async fn test_metadata_exchange_config_default() {
    let cfg = MetadataExchangeConfig::default();
    assert_eq!(cfg.max_peers_to_try, 5);
    assert_eq!(cfg.piece_size, 16 * 1024);
}

#[tokio::test]
async fn test_extract_compact_peers_from_mock_response() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let peer_bytes: Vec<u8> = vec![192, 168, 1, 1, 0x1F, 0x90];
    let mut r_dict = BTreeMap::new();
    r_dict.insert(
        b"values".to_vec(),
        BencodeValue::List(vec![BencodeValue::Bytes(peer_bytes)]),
    );

    let msg = aria2_protocol::bittorrent::dht::message::DhtMessage::new_response(
        vec![1, 2],
        BencodeValue::Dict(r_dict),
    );

    let peers = extract_compact_peers_from_response(&msg);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].port(), 8080);
}

#[tokio::test]
async fn test_e2e_magnet_metadata_exchange_with_mock_seeder() {
    let torrent_name = "e2e_test_file";
    let total_size: u64 = 1024 * 64; // 64KB
    let piece_length: u32 = 32 * 1024; // 32KB
    let tracker_url = "http://tracker.example.com:6969/announce";

    let torrent_data = build_test_torrent(torrent_name, total_size, piece_length, tracker_url);

    let meta = TorrentMeta::parse(&torrent_data).expect("Failed to parse test torrent");
    let info_hash = meta.info_hash.bytes;

    assert_eq!(meta.info.name, torrent_name);
    assert_eq!(meta.total_size(), total_size);

    let num_pieces = total_size.div_ceil(piece_length as u64);
    assert!(num_pieces > 1, "Test torrent should have multiple pieces");

    let mut collector = MetadataCollector::new(torrent_data.len() as u64, 16 * 1024).unwrap();
    assert!(!collector.is_complete());

    let metadata_piece_size = 16 * 1024;
    let metadata_num_pieces = torrent_data.len().div_ceil(metadata_piece_size);

    for i in 0..metadata_num_pieces {
        let start = i * metadata_piece_size;
        let end = std::cmp::min(start + metadata_piece_size, torrent_data.len());
        if start < torrent_data.len() {
            collector.add_piece(i as u32, &torrent_data[start..end]);
        }
    }

    assert!(
        collector.is_complete(),
        "MetadataCollector should be complete after all pieces"
    );
    let assembled = collector
        .into_bytes()
        .expect("Should assemble successfully");

    assert_eq!(
        assembled.len(),
        torrent_data.len(),
        "Assembled metadata should match original size"
    );
    assert_eq!(
        &assembled[..],
        &torrent_data[..],
        "Assembled content should match original"
    );

    let reassembled_meta =
        TorrentMeta::parse(&assembled).expect("Should parse reassembled metadata");
    assert_eq!(
        reassembled_meta.info_hash.bytes, info_hash,
        "Info hash should be preserved through assembly"
    );
    assert_eq!(
        reassembled_meta.info.name, torrent_name,
        "Name should be preserved"
    );

    info!(
        "E2E metadata exchange simulation completed: {} bytes assembled from {} pieces",
        total_size, num_pieces
    );
}

#[tokio::test]
async fn test_e2e_magnet_metadata_exchange_produces_valid_torrent_file() {
    let dir = tmp_dir();
    let torrent_name = "magnet_to_torrent_test";
    let total_size: u64 = 2048; // 2KB small file
    let piece_length: u32 = 1024;
    let tracker_url = "http://tracker.test.com/announce";

    let torrent_data = build_test_torrent(torrent_name, total_size, piece_length, tracker_url);
    let meta = TorrentMeta::parse(&torrent_data).expect("Failed to parse test torrent");
    let info_hash = meta.info_hash.bytes;

    let mut collector = MetadataCollector::new(torrent_data.len() as u64, 16 * 1024).unwrap();
    collector.add_piece(0, &torrent_data);

    assert!(
        collector.is_complete(),
        "Single-piece metadata should be complete"
    );
    let assembled = collector
        .into_bytes()
        .expect("Should assemble single-piece metadata");

    let torrent_path = dir.path().join("downloaded.torrent");
    std::fs::write(&torrent_path, &assembled).expect("Failed to write .torrent file");

    assert!(torrent_path.exists(), ".torrent file should be created");

    let reloaded = std::fs::read(&torrent_path).expect("Failed to read .torrent file");
    assert_eq!(reloaded.len(), assembled.len());

    let reloaded_meta = TorrentMeta::parse(&reloaded).expect("Failed to parse saved .torrent file");
    assert_eq!(
        reloaded_meta.info_hash.bytes, info_hash,
        "Saved torrent should have correct info_hash"
    );
    assert_eq!(
        reloaded_meta.total_size(),
        total_size,
        "Saved torrent should have correct total size"
    );
}

#[tokio::test]
async fn test_e2e_metadata_exchange_extension_handshake_cycle() {
    let torrent_data =
        build_test_torrent("handshake_test", 512, 256, "http://tracker.test/announce");
    let meta = TorrentMeta::parse(&torrent_data).expect("Failed to parse torrent");
    let info_hash = meta.info_hash.bytes;

    let mut hs = ExtensionHandshake::new();
    hs.with_metadata_size(torrent_data.len() as u32);
    let encoded_bytes = hs.to_bytes();

    let parsed =
        ExtensionHandshake::from_bytes(&encoded_bytes).expect("Should parse extension handshake");
    assert_eq!(parsed.metadata_size(), Some(torrent_data.len() as u32));
    assert!(
        parsed.ut_metadata_id().is_some(),
        "Should have ut_metadata extension ID"
    );

    let session = MetadataExchangeSession::new(MetadataExchangeConfig::default());

    let empty_peers: Vec<SocketAddr> = vec![];
    let result = session.fetch_metadata(&info_hash, &empty_peers).await;
    assert!(result.is_err(), "Should fail with no peers");
    match result.unwrap_err() {
        MetadataExchangeError::NoPeersAvailable => {}
        other => panic!("Expected NoPeersAvailable, got {:?}", other),
    }

    info!("Extension handshake cycle test completed successfully");
}

#[tokio::test]
async fn test_e2e_metadata_exchange_over_peer_wire() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let torrent_data = build_test_torrent(
        "metadata_wire_test",
        512,
        256,
        "http://tracker.test/announce",
    );
    let meta = TorrentMeta::parse(&torrent_data).expect("Failed to parse torrent");
    let info_hash = meta.info_hash.bytes;
    let (root, consumed) = BencodeValue::decode(&torrent_data).expect("decode test torrent");
    assert_eq!(consumed, torrent_data.len());
    let info_metadata = root
        .dict_get(b"info")
        .expect("test torrent should contain info dictionary")
        .encode();
    let peer =
        MockBtPeerServer::start_with_metadata(info_hash, Vec::new(), Some(info_metadata.clone()))
            .await;

    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        max_peers_to_try: 1,
        connect_timeout: std::time::Duration::from_secs(2),
        request_timeout: std::time::Duration::from_secs(2),
        max_attempts: 1,
        ..MetadataExchangeConfig::default()
    });
    let received = session
        .fetch_metadata(&info_hash, &[peer.addr()])
        .await
        .expect("metadata exchange should complete over the peer wire");

    assert_eq!(received, info_metadata);
}

#[tokio::test]
async fn test_e2e_metadata_exchange_rejects_peer_without_extended_capability() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let info_hash = [0x46u8; 20];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback peer");
    let peer_addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept metadata peer");
        let mut request = [0u8; 68];
        stream
            .read_exact(&mut request)
            .await
            .expect("read client's BitTorrent handshake");
        assert_eq!(&request[28..48], &info_hash);

        let mut response = Handshake::new(&info_hash, &[0x47u8; 20]);
        response.reserved[5] &= !0x10;
        stream
            .write_all(&response.to_bytes())
            .await
            .expect("send handshake without BEP 10 capability");

        let mut frame_length = [0u8; 4];
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            stream.read_exact(&mut frame_length),
        )
        .await
        {
            Ok(Ok(_)) => {
                let frame_size = u32::from_be_bytes(frame_length) as usize;
                let mut payload = vec![0u8; frame_size];
                if frame_size == 0 || stream.read_exact(&mut payload).await.is_err() {
                    return false;
                }
                let sent_extended = payload.first() == Some(&20);
                if sent_extended {
                    let mut next_frame = [0u8; 4];
                    let _ = tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        stream.read_exact(&mut next_frame),
                    )
                    .await;
                }
                sent_extended
            }
            Ok(Err(_)) | Err(_) => false,
        }
    });

    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        max_peers_to_try: 1,
        connect_timeout: std::time::Duration::from_secs(1),
        request_timeout: std::time::Duration::from_secs(4),
        max_attempts: 1,
        ..MetadataExchangeConfig::default()
    });
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        session.fetch_metadata(&info_hash, &[peer_addr]),
    )
    .await;
    let elapsed = started.elapsed();
    assert!(
        !peer.await.expect("peer observer should finish"),
        "metadata client must not send an Extended frame without negotiating BEP 10"
    );
    let result =
        result.expect("unsupported peer should be rejected before metadata request timeout");
    let error = result.expect_err("peer without BEP 10 cannot serve ut_metadata");
    assert!(matches!(
        error,
        MetadataExchangeError::AllPeersFailed { attempts: 1, .. }
    ));
    assert!(error.to_string().contains("unsupported"));
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "peer classification took {elapsed:?}, request timeout is 4 seconds"
    );
}

#[tokio::test]
async fn test_e2e_magnet_metadata_resolution_with_udp_only_tracker() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let torrent_data = build_test_torrent(
        "udp_only_magnet",
        512,
        256,
        "http://tracker.invalid/announce",
    );
    let meta = TorrentMeta::parse(&torrent_data).expect("parse test torrent");
    let (root, consumed) = BencodeValue::decode(&torrent_data).expect("decode test torrent");
    assert_eq!(consumed, torrent_data.len());
    let info_metadata = root
        .dict_get(b"info")
        .expect("test torrent should contain info dictionary")
        .encode();
    let peer = MockBtPeerServer::start_with_metadata(
        meta.info_hash.bytes,
        Vec::new(),
        Some(info_metadata),
    )
    .await;
    let tracker = MockUdpTracker::start_with_peers(vec![peer.addr()]).await;
    let dir = tmp_dir();
    let magnet = format!(
        "magnet:?xt=urn:btih:{}&dn=udp_only_magnet&tr={}",
        meta.info_hash.as_hex(),
        tracker.url()
    );
    let options = DownloadOptions {
        enable_dht: false,
        bt_metadata_only: true,
        bt_save_metadata: true,
        bt_tracker_timeout: 3,
        bt_tracker_connect_timeout: 3,
        ..DownloadOptions::default()
    };
    let mut command =
        MagnetDownloadCommand::new(GroupId::new(7001), &magnet, &options, dir.path().to_str())
            .expect("create UDP-only magnet command");

    tokio::time::timeout(std::time::Duration::from_secs(10), command.execute())
        .await
        .expect("UDP-only metadata lookup should finish promptly")
        .expect("UDP tracker should discover the metadata peer");

    let saved_metadata = dir
        .path()
        .join(format!("{}.torrent", meta.info_hash.as_hex()));
    let saved = std::fs::read(saved_metadata).expect("metadata should be saved");
    assert_eq!(
        TorrentMeta::parse(&saved)
            .expect("saved metadata should parse")
            .info_hash
            .bytes,
        meta.info_hash.bytes
    );
}

#[tokio::test]
async fn test_e2e_metadata_exchange_uses_configured_source_address() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;

    let torrent_data = build_test_torrent(
        "metadata_source_binding_test",
        512,
        256,
        "http://tracker.test/announce",
    );
    let meta = TorrentMeta::parse(&torrent_data).expect("parse test torrent");
    let info_hash = meta.info_hash.bytes;
    let (root, _) = BencodeValue::decode(&torrent_data).expect("decode test torrent");
    let info_metadata = root
        .dict_get(b"info")
        .expect("test torrent should contain info dictionary")
        .encode();
    let peer =
        MockBtPeerServer::start_with_metadata(info_hash, Vec::new(), Some(info_metadata)).await;
    let policy = std::sync::Arc::new(OutboundNetworkPolicy::single(
        "127.0.0.2".parse().expect("loopback source address"),
    ));
    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        max_peers_to_try: 1,
        connect_timeout: std::time::Duration::from_secs(2),
        request_timeout: std::time::Duration::from_secs(2),
        max_attempts: 1,
        ..MetadataExchangeConfig::default()
    })
    .with_outbound_network_policy(policy);

    session
        .fetch_metadata(&info_hash, &[peer.addr()])
        .await
        .expect("metadata exchange should complete over the configured source");

    let accepted_peers = peer.accepted_peers().await;
    assert_eq!(accepted_peers.len(), 1);
    assert_eq!(
        accepted_peers[0].ip(),
        "127.0.0.2".parse::<std::net::IpAddr>().unwrap()
    );
}

#[tokio::test]
async fn test_e2e_metadata_exchange_multiple_pieces() {
    let large_size: u64 = 100 * 1024; // 100KB to ensure multiple pieces
    let piece_len: u32 = 16 * 1024; // 16KB pieces -> ~7 pieces
    let torrent_data = build_test_torrent(
        "multi_piece_test",
        large_size,
        piece_len,
        "http://tracker.test/announce",
    );
    let meta = TorrentMeta::parse(&torrent_data).expect("Failed to parse torrent");
    let info_hash = meta.info_hash.bytes;

    let expected_num_pieces = large_size.div_ceil(piece_len as u64);
    assert!(expected_num_pieces > 1, "Test should use multiple pieces");

    let mut collector = MetadataCollector::new(torrent_data.len() as u64, 16 * 1024).unwrap();
    assert!(!collector.is_complete());

    let metadata_piece_size = 16 * 1024;
    let metadata_num_pieces = torrent_data.len().div_ceil(metadata_piece_size);

    for i in 0..metadata_num_pieces {
        let start = i * metadata_piece_size;
        let end = std::cmp::min(start + metadata_piece_size, torrent_data.len());
        if start < torrent_data.len() {
            assert!(
                !collector.is_complete(),
                "Should not be complete before piece {}",
                i
            );
            collector.add_piece(i as u32, &torrent_data[start..end]);
        }
    }

    assert!(
        collector.is_complete(),
        "Should be complete after all {} pieces",
        metadata_num_pieces
    );
    let assembled = collector
        .into_bytes()
        .expect("Should assemble multi-piece metadata");
    assert_eq!(
        assembled.len(),
        torrent_data.len(),
        "All pieces should assemble correctly"
    );

    let reassembled_meta =
        TorrentMeta::parse(&assembled).expect("Should parse reassembled multi-piece metadata");
    assert_eq!(
        reassembled_meta.info_hash.bytes, info_hash,
        "Info hash preserved in multi-piece assembly"
    );

    info!(
        "Multi-piece exchange test completed: {} bytes from {} metadata pieces",
        torrent_data.len(),
        metadata_num_pieces
    );
}

#[tokio::test]
async fn test_e2e_metadata_exchange_error_unsupported_peer() {
    let info_hash = [0xABu8; 20];

    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        connect_timeout: std::time::Duration::from_millis(100),
        request_timeout: std::time::Duration::from_millis(100),
        ..MetadataExchangeConfig::default()
    });

    let unreachable_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
    let result = session
        .fetch_metadata(&info_hash, &[unreachable_addr])
        .await;

    assert!(result.is_err(), "Should fail with unreachable peer");
    match result.unwrap_err() {
        MetadataExchangeError::PeerConnectFailed { .. }
        | MetadataExchangeError::PeerTimeout { .. }
        | MetadataExchangeError::AllPeersFailed { .. } => {}
        other => panic!("Expected connection/timeout error, got {:?}", other),
    }
}

#[tokio::test]
async fn test_e2e_metadata_collector_assembles_complete_torrent() {
    let total_size: u64 = 4096;
    let piece_size: u32 = 1024;
    let num_pieces = (total_size / piece_size as u64) as u32;

    let mut collector = MetadataCollector::new(total_size, piece_size).unwrap();
    assert!(!collector.is_complete());

    for i in 0..num_pieces {
        let start = (i as u64) * piece_size as u64;
        let end = std::cmp::min(start + piece_size as u64, total_size);
        let piece_data: Vec<u8> = (start..end).map(|b| (b % 256) as u8).collect();

        collector.add_piece(i, &piece_data);

        if i < num_pieces - 1 {
            assert!(
                !collector.is_complete(),
                "Should not be complete until last piece"
            );
        }
    }

    assert!(
        collector.is_complete(),
        "Should be complete after all pieces added"
    );

    let assembled = collector
        .into_bytes()
        .expect("Should assemble successfully");
    assert_eq!(
        assembled.len(),
        total_size as usize,
        "Assembled size should match total size"
    );

    for (i, &byte) in assembled.iter().enumerate() {
        let expected = (i as u64 % 256) as u8;
        assert_eq!(byte, expected, "Byte at index {} mismatch", i);
    }
}

#[tokio::test]
async fn test_e2e_metadata_exchange_reject_handling() {
    let info_hash = [0xCCu8; 20];

    let session = MetadataExchangeSession::new(MetadataExchangeConfig {
        connect_timeout: std::time::Duration::from_millis(50),
        request_timeout: std::time::Duration::from_millis(50),
        max_attempts: 1,
        ..MetadataExchangeConfig::default()
    });

    let bad_addr: SocketAddr = "127.0.0.1:2".parse().unwrap();
    let result = session.fetch_metadata(&info_hash, &[bad_addr]).await;

    assert!(result.is_err(), "Should fail with bad peer address");
}
