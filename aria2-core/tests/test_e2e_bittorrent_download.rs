mod fixtures {
    pub mod test_torrent_builder;
    pub mod mock_tracker;
    pub mod mock_bt_peer;
}
use fixtures::test_torrent_builder::{build_test_torrent, expected_piece_data};
use fixtures::mock_tracker::MockTrackerServer;
use fixtures::mock_bt_peer::MockBtPeerServer;
use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group::{GroupId, DownloadOptions};

fn tmp_dir() -> tempfile::TempDir { tempfile::tempdir().unwrap() }

#[tokio::test]
async fn test_e2e_bt_peer_connection_raw() {
    use aria2_protocol::bittorrent::peer::connection::{PeerConnection, PeerAddr};
    use aria2_protocol::bittorrent::message::types::{PieceBlockRequest, BtMessage};

    let info_hash = [1u8; 20];
    let piece_data = vec![vec![0xABu8; 512]];
    let peer = MockBtPeerServer::start(info_hash, piece_data).await;
    let peer_addr = PeerAddr::new("127.0.0.1", peer.addr().port());

    eprintln!("[RAW] Connecting to {}:{}", peer_addr.ip, peer_addr.port);
    let start = std::time::Instant::now();

    match tokio::time::timeout(std::time::Duration::from_secs(10), PeerConnection::connect(&peer_addr, &info_hash)).await {
        Ok(Ok(mut conn)) => {
            eprintln!("[RAW] Connected in {}ms", start.elapsed().as_millis());

            conn.send_unchoke().await.expect("send_unchoke failed");
            conn.send_interested().await.expect("send_interested failed");
            conn.send_bitfield(vec![0x00]).await.expect("send_bitfield failed");

            eprintln!("[RAW] Waiting for Unchoke...");
            for _ in 0..10 {
                match tokio::time::timeout(std::time::Duration::from_secs(2), conn.read_message()).await {
                    Ok(Ok(Some(BtMessage::Unchoke))) => { eprintln!("[RAW] Got Unchoke!"); break; }
                    Ok(Ok(Some(m))) => { eprintln!("[RAW] Got other: {:?}", m.message_id()); }
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
                        Ok(Some(m)) => { eprintln!("[RAW] Non-piece msg: {:?}", m.message_id()); }
                        Ok(None) => { eprintln!("[RAW] EOF"); return Err(()); }
                        Err(e) => { eprintln!("[RAW] Error: {}", e); return Err(()); }
                    }
                }
                Err(())
            }).await {
                Ok(Ok((idx, beg, dat))) => {
                    eprintln!("[RAW] GOT PIECE! idx={}, begin={}, len={}", idx, beg, dat.len());
                    assert_eq!(idx, 0);
                    assert_eq!(beg, 0);
                    assert_eq!(dat.len(), 512);
                    assert!(dat.iter().all(|&b| b == 0xAB));
                    eprintln!("[RAW] ALL CHECKS PASSED!");
                }
                Ok(Err(())) => { panic!("[RAW] No Piece response received"); }
                Err(_) => { panic!("[RAW] Timed out waiting for Piece"); }
            }
        }
        Ok(Err(e)) => { panic!("[RAW] Connect failed: {}", e); }
        Err(_) => { panic!("[RAW] Connect timed out"); }
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

#[tokio::test]
async fn test_e2e_bt_small_torrent_download() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let tracker_url = tracker.announce_url();

    let torrent_data = build_test_torrent("test.bin", 1024, 512, &tracker_url);
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
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
            seed_time: Some(0), // 禁用 seeding
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    ).expect("BtDownloadCommand 创建失败");

    match tokio::time::timeout(std::time::Duration::from_secs(15), cmd.execute()).await {
        Ok(Ok(())) => eprintln!("[DL] Download OK!"),
        Ok(Err(e)) => { eprintln!("[DL] Download ERROR: {}", e); panic!("BT download failed: {}", e); }
        Err(_) => { eprintln!("[DL] Download TIMEOUT after 15s"); panic!("BT download timed out"); }
    }

    let output_path = dir.path().join("test.bin");
    assert!(output_path.exists(), "输出文件不存在: {}", output_path.display());

    let data = std::fs::read(&output_path).expect("读取下载文件失败");
    assert_eq!(data.len(), 1024, "文件大小不匹配");
    assert_eq!(&data[0..4], &[0u8, 1, 2, 3], "内容前4字节应为0,1,2,3");
    assert_eq!(&data[1020..], &[252u8, 253, 254, 255], "内容最后4字节应为252,253,254,255");
}

#[tokio::test]
async fn test_e2e_bt_medium_torrent_download() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let tracker_url = tracker.announce_url();

    let total_size: u64 = 64 * 1024;
    let piece_length: u32 = 16 * 1024;

    let torrent_data = build_test_torrent("data.bin", total_size, piece_length, &tracker_url);
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;

    let num_pieces = meta.num_pieces();
    let mut pieces = Vec::with_capacity(num_pieces as usize);
    for i in 0..num_pieces { pieces.push(expected_piece_data(i as u32, piece_length, total_size)); }

    let peer = MockBtPeerServer::start(info_hash, pieces).await;
    let peer_port = peer.addr().port();

    let tracker_with_peer = MockTrackerServer::start(peer_port).await;
    let final_tracker_url = tracker_with_peer.announce_url();

    let torrent_for_cmd = build_test_torrent("data.bin", total_size, piece_length, &final_tracker_url);

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(101),
        &torrent_for_cmd,
        &DownloadOptions {
            seed_time: Some(0),
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    ).unwrap();

    cmd.execute().await.expect("BT medium下载失败");

    let output_path = dir.path().join("data.bin");
    assert!(output_path.exists());
    let data = std::fs::read(&output_path).unwrap();
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
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;

    let piece0 = expected_piece_data(0, 512, 1024);
    let piece1 = expected_piece_data(1, 512, 1024);

    let peer = MockBtPeerServer::start(info_hash, vec![piece0, piece1]).await;
    let peer_port = peer.addr().port();

    let tracker_with_peer = MockTrackerServer::start(peer_port).await;
    let final_tracker_url = tracker_with_peer.announce_url();

    let torrent_for_cmd = build_test_torrent("progress.bin", 1024, 512, &final_tracker_url);

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(300), &torrent_for_cmd, &DownloadOptions {
            seed_time: Some(0),
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    ).unwrap();

    let progress_before = cmd.group().await.progress().await;
    assert!((progress_before - 0.0).abs() < f64::EPSILON, "下载前进度应为0");

    cmd.execute().await.expect("BT下载失败");

    let progress_after = cmd.group().await.progress().await;
    assert!((progress_after - 100.0).abs() < 1.0, "下载后进度应接近100%, got: {}", progress_after);

    let status = cmd.group().await.status().await;
    assert!(status.is_completed());
}
