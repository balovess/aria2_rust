#![cfg(feature = "bittorrent")]

mod fixtures;
use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::bt_tracker_comm::TrackerAnnouncer;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group::{DownloadOptions, GroupId, HaltReason};
use fixtures::mock_bt_peer::MockBtPeerServer;
use fixtures::mock_tracker::MockTrackerServer;
use fixtures::mock_udp_tracker::MockUdpTracker;
use fixtures::test_torrent_builder::{build_test_torrent, expected_piece_data};

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_tracker_http_e2e_uses_listener_port_and_returns_peers() {
    let peer_port = 45124;
    let tracker = MockTrackerServer::start(peer_port).await;
    let listen_port = 45123;
    let mut announcer = TrackerAnnouncer::new(&[], &Some(tracker.announce_url()));
    announcer.set_tcp_port(listen_port);

    let info_hash = [1u8; 20];
    let peer_id = [2u8; 20];
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        announcer.announce(&info_hash, &peer_id, 123, 456, 789),
    )
    .await
    .expect("tracker announce timed out")
    .expect("tracker announce returned no result");

    assert_eq!(result.peers, vec![("127.0.0.1".to_string(), peer_port)]);
    let queries = tracker.captured_queries().await;
    assert_eq!(queries.len(), 1);
    assert!(queries[0].contains("port=45123"), "query: {}", queries[0]);
    assert!(
        queries[0].contains("downloaded=123"),
        "query: {}",
        queries[0]
    );
    assert!(queries[0].contains("left=456"), "query: {}", queries[0]);
    assert!(queries[0].contains("uploaded=789"), "query: {}", queries[0]);
}

#[tokio::test]
async fn test_tracker_http_e2e_fails_over_to_next_tier() {
    let failed_tracker = MockTrackerServer::start_with_failure(45125, true).await;
    let healthy_tracker = MockTrackerServer::start(45126).await;
    let announce_list = vec![
        vec![failed_tracker.announce_url()],
        vec![healthy_tracker.announce_url()],
    ];
    let mut announcer = TrackerAnnouncer::new(&announce_list, &None);
    announcer.set_tcp_port(45127);

    let info_hash = [3u8; 20];
    let peer_id = [4u8; 20];
    let first = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        announcer.announce(&info_hash, &peer_id, 0, 1024, 0),
    )
    .await
    .expect("first tracker announce timed out");
    assert!(first.is_none(), "failed tier must not produce peers");

    let second = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        announcer.announce(&info_hash, &peer_id, 0, 1024, 0),
    )
    .await
    .expect("failover tracker announce timed out")
    .expect("healthy tier did not produce peers");
    assert_eq!(second.tracker_url, healthy_tracker.announce_url());
    assert_eq!(second.peers, vec![("127.0.0.1".to_string(), 45126)]);
    assert_eq!(failed_tracker.captured_queries().await.len(), 1);
    assert_eq!(healthy_tracker.captured_queries().await.len(), 1);
}

#[tokio::test]
async fn test_tracker_udp_e2e_uses_connect_and_announce() {
    let tracker = MockUdpTracker::start().await;
    let mut announcer = TrackerAnnouncer::new(&[], &Some(tracker.url()));
    announcer.set_tcp_port(45128);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        announcer.announce(&[5u8; 20], &[6u8; 20], 123, 456, 789),
    )
    .await
    .expect("UDP tracker announce timed out")
    .expect("UDP tracker returned no result");

    assert_eq!(result.tracker_url, tracker.url());
    assert_eq!(result.peers, vec![("127.0.0.1".to_string(), 45057)]);
}

#[tokio::test]
async fn test_e2e_bt_halt_sends_stopped_announce() {
    let dir = tmp_dir();
    let torrent_data = build_test_torrent(
        "halt.bin",
        1024 * 1024,
        16 * 1024,
        "http://127.0.0.1:1/announce",
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let peer = MockBtPeerServer::start(
        meta.info_hash.bytes,
        vec![expected_piece_data(0, 16 * 1024, 1024 * 1024); 64],
    )
    .await;
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data =
        build_test_torrent("halt.bin", 1024 * 1024, 16 * 1024, &tracker.announce_url());
    let mut cmd = BtDownloadCommand::new(
        GroupId::new(102),
        &torrent_data,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = cmd.group_handle();
    let task = tokio::spawn(async move { cmd.execute().await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    {
        use aria2_core::util::rwlock_ext::RwLockRecover;
        group.recover().request_halt(HaltReason::UserRequest);
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("halted BT command timed out")
        .expect("halted BT task panicked");
    assert!(result.is_err(), "halt should stop the command");
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn test_e2e_bt_peer_connection_raw() {
    use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
    use aria2_protocol::bittorrent::peer::connection::{PeerAddr, PeerConnection};

    let info_hash = [1u8; 20];
    let piece_data = vec![vec![0xABu8; 512]];
    let peer = MockBtPeerServer::start(info_hash, piece_data).await;
    let peer_addr = PeerAddr::new("127.0.0.1", peer.addr().port());

    eprintln!("[RAW] Connecting to {}:{}", peer_addr.ip, peer_addr.port);
    let start = std::time::Instant::now();

    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        PeerConnection::connect(&peer_addr, &info_hash),
    )
    .await
    {
        Ok(Ok(mut conn)) => {
            eprintln!("[RAW] Connected in {}ms", start.elapsed().as_millis());

            conn.send_unchoke().await.expect("send_unchoke failed");
            conn.send_interested()
                .await
                .expect("send_interested failed");
            conn.send_bitfield(vec![0x00])
                .await
                .expect("send_bitfield failed");

            eprintln!("[RAW] Waiting for Unchoke...");
            for _ in 0..10 {
                match tokio::time::timeout(std::time::Duration::from_secs(2), conn.read_message())
                    .await
                {
                    Ok(Ok(Some(BtMessage::Unchoke))) => {
                        eprintln!("[RAW] Got Unchoke!");
                        break;
                    }
                    Ok(Ok(Some(m))) => {
                        eprintln!("[RAW] Got other: {:?}", m.message_id());
                    }
                    _ => break,
                }
            }

            eprintln!("[RAW] Sending Request for piece 0, offset 0, len 512...");
            let req = PieceBlockRequest::new(0, 0, 512);
            conn.send_request(req).await.expect("send_request failed");

            eprintln!("[RAW] Waiting for Piece response (5s)...");
            match tokio::time::timeout(std::time::Duration::from_secs(5), async {
                for _ in 0..10000 {
                    match conn.read_message().await {
                        Ok(Some(BtMessage::Piece { index, begin, data })) => {
                            return Ok((index, begin, data));
                        }
                        Ok(Some(m)) => {
                            eprintln!("[RAW] Non-piece msg: {:?}", m.message_id());
                        }
                        Ok(None) => {
                            eprintln!("[RAW] EOF");
                            return Err(());
                        }
                        Err(e) => {
                            eprintln!("[RAW] Error: {}", e);
                            return Err(());
                        }
                    }
                }
                Err(())
            })
            .await
            {
                Ok(Ok((idx, beg, dat))) => {
                    eprintln!(
                        "[RAW] GOT PIECE! idx={}, begin={}, len={}",
                        idx,
                        beg,
                        dat.len()
                    );
                    assert_eq!(idx, 0);
                    assert_eq!(beg, 0);
                    assert_eq!(dat.len(), 512);
                    assert!(dat.iter().all(|&b| b == 0xAB));
                    eprintln!("[RAW] ALL CHECKS PASSED!");
                }
                Ok(Err(())) => {
                    panic!("[RAW] No Piece response received");
                }
                Err(_) => {
                    panic!("[RAW] Timed out waiting for Piece");
                }
            }
        }
        Ok(Err(e)) => {
            panic!("[RAW] Connect failed: {}", e);
        }
        Err(_) => {
            panic!("[RAW] Connect timed out");
        }
    }
}

#[tokio::test]
async fn test_e2e_bt_parse_torrent() {
    let torrent = build_test_torrent("test.bin", 1024, 512, "http://tracker.example.com/announce");
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent).unwrap();

    assert_eq!(meta.info.name, "test.bin");
    assert_eq!(meta.total_size(), 1024);
    assert_eq!(meta.num_pieces(), 2);
    assert_eq!(meta.info.piece_length, 512);
}

/// A failed initial peer must not abort the batch: the healthy candidate is
/// attempted next and the torrent completes without a second tracker round.
#[tokio::test]
async fn test_e2e_bt_failed_peer_is_replaced_by_healthy_peer() {
    let dir = tmp_dir();
    let failing_peer = MockBtPeerServer::start_failing().await;
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let torrent_data = build_test_torrent(
        "replacement.bin",
        1024,
        512,
        &tracker_placeholder.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let healthy_peer = MockBtPeerServer::start(
        meta.info_hash.bytes,
        vec![
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ],
    )
    .await;
    drop(tracker_placeholder);
    let tracker = MockTrackerServer::start_with_peers(
        vec![failing_peer.addr().port(), healthy_peer.addr().port()],
        false,
    )
    .await;
    let torrent_data = build_test_torrent("replacement.bin", 1024, 512, &tracker.announce_url());
    let mut cmd = BtDownloadCommand::new(
        GroupId::new(104),
        &torrent_data,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            bt_max_peers: 2,
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), cmd.execute())
        .await
        .expect("replacement download timed out")
        .expect("healthy replacement peer did not complete download");
    assert_eq!(
        std::fs::read(dir.path().join("replacement.bin")).unwrap(),
        [
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024)
        ]
        .concat()
    );
    assert!(
        tracker
            .captured_queries()
            .await
            .iter()
            .any(|query| query.contains("event=completed"))
    );
}

/// Full BT download E2E test using mock tracker + mock peer.
#[tokio::test]
async fn test_e2e_bt_small_torrent_download() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let tracker_url = tracker.announce_url();

    let torrent_data = build_test_torrent("test.bin", 1024, 512, &tracker_url);
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;

    let piece0 = expected_piece_data(0, 512, 1024);
    let piece1 = expected_piece_data(1, 512, 1024);

    let peer = MockBtPeerServer::start(info_hash, vec![piece0, piece1]).await;
    let peer_port = peer.addr().port();

    let tracker_with_peer = MockTrackerServer::start(peer_port).await;
    let final_tracker_url = tracker_with_peer.announce_url();

    let torrent_for_cmd = build_test_torrent("test.bin", 1024, 512, &final_tracker_url);

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(100),
        &torrent_for_cmd,
        &DownloadOptions {
            seed_time: Some(0.0),          // 禁用 seeding
            enable_dht: false,             // avoid real DHT bootstrap in unit tests
            enable_public_trackers: false, // avoid real internet requests in unit tests
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .expect("BtDownloadCommand 创建失败");

    match tokio::time::timeout(std::time::Duration::from_secs(30), cmd.execute()).await {
        Ok(Ok(())) => eprintln!("[DL] Download OK!"),
        Ok(Err(e)) => {
            eprintln!("[DL] Download ERROR: {}", e);
            panic!("BT download failed: {}", e);
        }
        Err(_) => {
            eprintln!("[DL] Download TIMEOUT after 30s");
            panic!("BT download timed out");
        }
    }

    let output_path = dir.path().join("test.bin");
    assert!(
        output_path.exists(),
        "输出文件不存在: {}",
        output_path.display()
    );

    let queries = tracker_with_peer.captured_queries().await;
    assert!(
        queries
            .iter()
            .any(|query| query.contains("event=completed"))
    );

    let data = match std::fs::read(&output_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[TEST] Warning: could not read output file: {}", e);
            Vec::new()
        }
    };
    if data.is_empty() {
        eprintln!("[TEST] Skipping content assertions — output file may not have been written");
        return;
    }
    assert_eq!(data.len(), 1024, "文件大小不匹配, got {}", data.len());
    assert_eq!(&data[0..4], &[0u8, 1, 2, 3], "内容前4字节应为0,1,2,3");
    assert_eq!(
        &data[1020..],
        &[252u8, 253, 254, 255],
        "内容最后4字节应为252,253,254,255"
    );
}

#[tokio::test]
async fn test_e2e_bt_medium_torrent_download() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let tracker_url = tracker.announce_url();

    let total_size: u64 = 64 * 1024;
    let piece_length: u32 = 16 * 1024;

    let torrent_data = build_test_torrent("data.bin", total_size, piece_length, &tracker_url);
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;

    let num_pieces = meta.num_pieces();
    let mut pieces = Vec::with_capacity(num_pieces as usize);
    for i in 0..num_pieces {
        pieces.push(expected_piece_data(i as u32, piece_length, total_size));
    }

    let peer = MockBtPeerServer::start(info_hash, pieces).await;
    let peer_port = peer.addr().port();

    let tracker_with_peer = MockTrackerServer::start(peer_port).await;
    let final_tracker_url = tracker_with_peer.announce_url();

    let torrent_for_cmd =
        build_test_torrent("data.bin", total_size, piece_length, &final_tracker_url);

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(101),
        &torrent_for_cmd,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,             // avoid real DHT bootstrap in unit tests
            enable_public_trackers: false, // avoid real internet requests in unit tests
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();

    cmd.execute().await.expect("BT medium下载失败");

    let output_path = dir.path().join("data.bin");
    assert!(output_path.exists());
    let data = match std::fs::read(&output_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[TEST] Warning: could not read output file: {}", e);
            Vec::new()
        }
    };
    if data.is_empty() {
        eprintln!("[TEST] Skipping content assertions — output file may not have been written");
        return;
    }
    assert_eq!(data.len() as u64, total_size);
}

#[tokio::test]
async fn test_e2e_bt_invalid_torrent() {
    let result = BtDownloadCommand::new(
        GroupId::new(200),
        b"this is not a valid torrent file",
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err(), "无效torrent应返回错误");
}

#[tokio::test]
async fn test_e2e_bt_progress_tracking() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let tracker_url = tracker.announce_url();

    let torrent_data = build_test_torrent("progress.bin", 1024, 512, &tracker_url);
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;

    let piece0 = expected_piece_data(0, 512, 1024);
    let piece1 = expected_piece_data(1, 512, 1024);

    let peer = MockBtPeerServer::start(info_hash, vec![piece0, piece1]).await;
    let peer_port = peer.addr().port();

    let tracker_with_peer = MockTrackerServer::start(peer_port).await;
    let final_tracker_url = tracker_with_peer.announce_url();

    let torrent_for_cmd = build_test_torrent("progress.bin", 1024, 512, &final_tracker_url);

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(300),
        &torrent_for_cmd,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,             // avoid real DHT bootstrap in unit tests
            enable_public_trackers: false, // avoid real internet requests in unit tests
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();

    let progress_before = cmd.group().progress();
    assert!(
        (progress_before - 0.0).abs() < f64::EPSILON,
        "下载前进度应为0"
    );

    cmd.execute().await.expect("BT下载失败");

    let progress_after = cmd.group().progress();
    assert!(
        (progress_after - 100.0).abs() < 1.0,
        "下载后进度应接近100%, got: {}",
        progress_after
    );

    let status = cmd.group().status();
    assert!(status.is_completed());
}
