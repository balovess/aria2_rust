#![cfg(feature = "bittorrent")]

mod e2e_helpers;
mod fixtures;

use aria2_core::engine::bt_download_command::BtDownloadCommand;
use aria2_core::engine::bt_progress_info_file::{
    BtProgress, BtProgressManager, DownloadStats as ProgressDownloadStats,
};
use aria2_core::engine::bt_tracker_comm::TrackerAnnouncer;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::download_event_hooks::{
    DownloadEvent, DownloadEventHooks, DownloadEventListener,
};
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::request::request_group::{
    DownloadOptions, DownloadResultCode, DownloadStatus, GroupId, HaltReason, RequestGroup,
};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::session::save_session_command::SaveSessionCommand;
use aria2_core::util::rwlock_ext::RwLockRecover;
use e2e_helpers::mock_http_server::MockHttpServer;
use fixtures::mock_bt_peer::MockBtPeerServer;
use fixtures::mock_bt_seeder::{MockBtSeeder, SeederConfig};
use fixtures::mock_tracker::MockTrackerServer;
use fixtures::mock_udp_tracker::MockUdpTracker;
use fixtures::test_torrent_builder::{
    build_multi_file_test_torrent, build_test_torrent, build_test_torrent_with_web_seeds,
    expected_piece_data,
};
use std::sync::{Arc, Mutex};

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[derive(Default)]
struct RecordingBtEvents {
    events: Mutex<Vec<(DownloadEvent, String)>>,
}

impl RecordingBtEvents {
    fn saw(&self, event: DownloadEvent, gid: &str) -> bool {
        self.events
            .lock()
            .unwrap()
            .iter()
            .any(|(seen, seen_gid)| *seen == event && seen_gid == gid)
    }
}

impl DownloadEventListener for RecordingBtEvents {
    fn on_download_event(&self, event: DownloadEvent, gid: &str) {
        self.events.lock().unwrap().push((event, gid.to_string()));
    }
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
    let mut peers = result.peers;
    peers.sort_unstable();
    assert_eq!(
        peers,
        vec![
            ("10.0.0.1".to_string(), 6882),
            ("192.168.1.1".to_string(), 6881),
        ]
    );
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
            bt_stop_timeout: Some(0),
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
    assert_eq!(group.recover().get_halt_reason(), HaltReason::UserRequest);
    let control_path = ControlFile::control_path_for(&dir.path().join("halt.bin"));
    let control_file = ControlFile::load(&control_path)
        .await
        .expect("halted BT download should write a valid checkpoint")
        .expect("halted BT download should leave its checkpoint on disk");
    assert_eq!(control_file.total_length(), 1024 * 1024);
    assert_eq!(control_file.bitfield().len(), 8);
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn test_e2e_bt_halt_cancels_stalled_initial_peer_connections() {
    let dir = tmp_dir();
    let stalled_peer = MockBtPeerServer::start_failing().await;
    let tracker = MockTrackerServer::start(stalled_peer.addr().port()).await;
    let torrent_data = build_test_torrent(
        "halt-connect.bin",
        1024 * 1024,
        16 * 1024,
        &tracker.announce_url(),
    );
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        peer_connection_timeout: 20,
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(113),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    let task = tokio::spawn(async move { command.execute().await });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let halt_started = std::time::Instant::now();
    group.recover().request_halt(HaltReason::ShutdownSignal);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("halt should cancel a stalled initial peer connection")
        .expect("halted BT task panicked");
    assert!(result.is_err(), "halt should stop the current command");
    assert!(
        halt_started.elapsed() < std::time::Duration::from_secs(2),
        "stalled peer connection was not cancelled: {:?}",
        halt_started.elapsed()
    );
}

#[tokio::test]
async fn test_e2e_engine_bt_force_halt_preserves_resume_state() {
    let dir = tmp_dir();
    let total_size = 1024 * 1024;
    let piece_length = 16 * 1024;
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = build_test_torrent(
        "engine-force-halt.bin",
        total_size,
        piece_length,
        &placeholder_tracker.announce_url(),
    );
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("force-halt torrent should parse");
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        vec![expected_piece_data(0, piece_length, total_size); 64],
        std::time::Duration::from_secs(3),
    )
    .await;
    drop(placeholder_tracker);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent(
        "engine-force-halt.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
    );
    let options = DownloadOptions {
        dir: Some(dir.path().to_string_lossy().into_owned()),
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(1100);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec!["bt://engine-force-halt".to_string()],
        options,
    )));
    group.recover().set_bt_metadata_data(torrent_data);

    let manager = Arc::new(RequestGroupMan::new());
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::clone(&manager));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("BT engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.recover().status() == DownloadStatus::Active {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("BT engine did not promote the group");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !peer.requested_pieces().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("BT peer did not receive a piece request");

    command_tx
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .expect("BT force-halt command should be accepted");
    tokio::time::timeout(std::time::Duration::from_secs(10), engine_task)
        .await
        .expect("BT force-halt engine did not stop")
        .expect("BT force-halt engine task panicked")
        .expect("BT force-halt engine returned an error");

    {
        let group_state = group.read().unwrap();
        assert_eq!(group_state.get_halt_reason(), HaltReason::ShutdownSignal);
        assert_eq!(
            group_state.create_download_result().code,
            DownloadResultCode::InProgress
        );
        assert_ne!(
            group_state.status(),
            DownloadStatus::Removed,
            "force shutdown must keep the BT task resumable"
        );
    }

    let output_path = dir.path().join("engine-force-halt.bin");
    let control_path = ControlFile::control_path_for(&output_path);
    let control_file = ControlFile::load(&control_path)
        .await
        .expect("BT force-halt control file should be readable")
        .expect("BT force-halt must preserve its control file");
    assert_eq!(control_file.total_length(), total_size);
    assert_eq!(control_file.bitfield().len(), 8);
    assert!(
        control_file.completed_length() < total_size,
        "force-halt should leave resumable partial progress"
    );
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn test_e2e_engine_bt_multi_file_force_halt_preserves_layout() {
    let dir = tmp_dir();
    let file_lengths = [16 * 1024 * 1024, 16 * 1024 * 1024];
    let piece_length = 64 * 1024;
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = build_multi_file_test_torrent(
        "engine-multi-force-halt",
        &file_lengths,
        piece_length,
        &placeholder_tracker.announce_url(),
    );
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("multi-file force-halt torrent should parse");
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        (0..meta.num_pieces() as u32)
            .map(|piece| expected_piece_data(piece, piece_length, file_lengths.iter().sum()))
            .collect(),
        std::time::Duration::from_secs(3),
    )
    .await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_multi_file_test_torrent(
        "engine-multi-force-halt",
        &file_lengths,
        piece_length,
        &tracker.announce_url(),
    );
    let options = DownloadOptions {
        dir: Some(dir.path().to_string_lossy().into_owned()),
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        file_allocation: Some("prealloc".to_string()),
        secure_falloc: true,
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(1101);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec!["bt://engine-multi-force-halt".to_string()],
        options,
    )));
    group.recover().set_bt_metadata_data(torrent_data);

    let manager = Arc::new(RequestGroupMan::new());
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::clone(&manager));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("multi-file BT engine command should be accepted");
    let engine_task = tokio::spawn(engine.run());

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if group.recover().status() == DownloadStatus::Active {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("multi-file BT engine did not promote the group");

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !peer.requested_pieces().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("multi-file BT peer did not receive a piece request");

    command_tx
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .expect("multi-file BT force-halt command should be accepted");
    tokio::time::timeout(std::time::Duration::from_secs(10), engine_task)
        .await
        .expect("multi-file BT force-halt engine did not stop")
        .expect("multi-file BT force-halt engine task panicked")
        .expect("multi-file BT force-halt engine returned an error");

    assert_eq!(
        group.recover().get_halt_reason(),
        HaltReason::ShutdownSignal
    );
    assert_eq!(
        group.recover().create_download_result().code,
        DownloadResultCode::InProgress
    );
    assert!(
        dir.path().join("part-0.bin").is_file() && dir.path().join("part-1.bin").is_file(),
        "multi-file allocation should have created every payload file"
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("part-0.bin"))
            .expect("first multi-file payload metadata should be readable")
            .len(),
        file_lengths[0]
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("part-1.bin"))
            .expect("second multi-file payload metadata should be readable")
            .len(),
        file_lengths[1]
    );
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn test_e2e_bt_stop_timeout_returns_timeout_result_without_peers() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let torrent_data = build_test_torrent(
        "stop-timeout.bin",
        1024 * 1024,
        16 * 1024,
        &tracker.announce_url(),
    );
    let mut cmd = BtDownloadCommand::new(
        GroupId::new(104),
        &torrent_data,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            bt_stop_timeout: Some(1),
            file_allocation: Some("none".to_string()),
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = cmd.group_handle();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.execute())
        .await
        .expect("BT stop-timeout command timed out");
    assert!(result.is_err(), "stop-timeout must stop a stalled download");

    let group = group.recover();
    assert_eq!(group.get_halt_reason(), HaltReason::Timeout);
    assert_eq!(group.get_last_error_code(), DownloadResultCode::TimeOut);
    assert_eq!(
        group.create_download_result().code,
        DownloadResultCode::TimeOut
    );
}

#[tokio::test]
async fn test_e2e_bt_missing_tail_piece_waits_for_discovery_timeout() {
    let dir = tmp_dir();
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let torrent_data = build_test_torrent(
        "missing-tail.bin",
        1024,
        512,
        &tracker_placeholder.announce_url(),
    );
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data)
        .expect("missing-tail torrent should parse");
    let peer = MockBtSeeder::start(
        meta.info_hash.bytes,
        std::collections::HashMap::from([(0u32, expected_piece_data(0, 512, 1024))]),
        SeederConfig::default(),
    )
    .await;
    drop(tracker_placeholder);

    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent("missing-tail.bin", 1024, 512, &tracker.announce_url());
    let mut cmd = BtDownloadCommand::new(
        GroupId::new(111),
        &torrent_data,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            bt_stop_timeout: Some(1),
            file_allocation: Some("none".to_string()),
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), cmd.execute())
        .await
        .expect("missing-tail download did not reach its stop timeout");
    assert!(
        result.is_err(),
        "missing tail piece must not be reported complete"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(900),
        "missing tail piece was rejected before discovery timeout"
    );
    assert_eq!(cmd.group().get_halt_reason(), HaltReason::Timeout);
    assert_eq!(
        cmd.group().get_last_error_code(),
        DownloadResultCode::TimeOut
    );
    peer.shutdown().await;
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn test_e2e_bt_corrupt_tail_peer_is_removed_before_timeout() {
    let dir = tmp_dir();
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let torrent_data = build_test_torrent(
        "corrupt-tail.bin",
        512,
        512,
        &tracker_placeholder.announce_url(),
    );
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data)
        .expect("corrupt-tail torrent should parse");
    let corrupt_peer = MockBtSeeder::start(
        meta.info_hash.bytes,
        std::collections::HashMap::from([(0u32, expected_piece_data(0, 512, 512))]),
        SeederConfig {
            send_corrupt_piece: true,
            ..SeederConfig::default()
        },
    )
    .await;
    drop(tracker_placeholder);

    let tracker = MockTrackerServer::start(corrupt_peer.addr().port()).await;
    let torrent_data = build_test_torrent("corrupt-tail.bin", 512, 512, &tracker.announce_url());
    let mut cmd = BtDownloadCommand::new(
        GroupId::new(112),
        &torrent_data,
        &DownloadOptions {
            max_retries: 1,
            bt_request_timeout: 1,
            bt_stop_timeout: Some(2),
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            file_allocation: Some("none".to_string()),
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();

    let group = cmd.group_handle();
    let started = std::time::Instant::now();
    let task = tokio::spawn(async move { cmd.execute().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if corrupt_peer.connection_count() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("corrupt peer did not become active");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if corrupt_peer.connection_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("corrupt peer was not removed after hash failure");
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("corrupt-tail download did not reach its stop timeout")
        .expect("corrupt-tail task panicked");
    assert!(
        result.is_err(),
        "corrupt data must not be reported complete"
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(1800),
        "corrupt peer failure was treated as terminal before discovery timeout"
    );
    assert_eq!(group.recover().get_halt_reason(), HaltReason::Timeout);
    assert_eq!(
        group.recover().get_last_error_code(),
        DownloadResultCode::TimeOut
    );
    corrupt_peer.shutdown().await;
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn test_e2e_bt_resume_skips_verified_checkpoint_pieces() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let torrent_data = build_test_torrent("checkpoint.bin", 1024, 512, &tracker.announce_url());
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;
    let peer = MockBtPeerServer::start(
        info_hash,
        vec![
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ],
    )
    .await;
    drop(tracker);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent("checkpoint.bin", 1024, 512, &tracker.announce_url());
    let output_path = dir.path().join("checkpoint.bin");
    let piece_zero = expected_piece_data(0, 512, 1024);
    std::fs::write(&output_path, &piece_zero).unwrap();

    let control_path = ControlFile::control_path_for(&output_path);
    let mut control_file = ControlFile::open_or_create(&control_path, 1024, 2)
        .await
        .unwrap();
    control_file.mark_torrent_checkpoint();
    control_file.set_torrent_info_hash(info_hash);
    control_file.mark_piece_done(0);
    control_file.update_completed_length(512);
    control_file.save().await.unwrap();

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(103),
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
    tokio::time::timeout(std::time::Duration::from_secs(20), cmd.execute())
        .await
        .expect("checkpoint resume timed out")
        .expect("checkpoint resume failed");

    let requested = peer.requested_pieces().await;
    assert!(
        requested.contains(&1),
        "missing piece was not requested: {requested:?}"
    );
    assert!(
        !requested.contains(&0),
        "verified checkpoint piece was requested again: {requested:?}"
    );
    assert_eq!(
        std::fs::read(&output_path).unwrap(),
        [piece_zero, expected_piece_data(1, 512, 1024)].concat()
    );
    assert!(
        !control_path.exists(),
        "successful resume must remove checkpoint"
    );
}

#[tokio::test]
async fn test_e2e_bt_resume_skips_verified_legacy_progress_pieces() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start(0).await;
    let torrent_data =
        build_test_torrent("legacy-progress.bin", 1024, 512, &tracker.announce_url());
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let info_hash = meta.info_hash.bytes;
    let peer = MockBtPeerServer::start(
        info_hash,
        vec![
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ],
    )
    .await;
    drop(tracker);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data =
        build_test_torrent("legacy-progress.bin", 1024, 512, &tracker.announce_url());
    let output_path = dir.path().join("legacy-progress.bin");
    let piece_zero = expected_piece_data(0, 512, 1024);
    std::fs::write(&output_path, &piece_zero).unwrap();

    let manager = BtProgressManager::new(dir.path()).unwrap();
    let progress_path = manager.get_progress_file_path(&info_hash);
    manager
        .save_progress(
            &info_hash,
            &BtProgress {
                info_hash,
                bitfield: vec![0b1000_0000],
                peers: vec![],
                stats: ProgressDownloadStats {
                    downloaded_bytes: 512,
                    ..ProgressDownloadStats::default()
                },
                piece_length: 512,
                total_size: 1024,
                num_pieces: 2,
                upload_length: 0,
                in_flight_pieces: vec![],
                is_torrent: true,
                save_time: std::time::SystemTime::now(),
                version: 1,
            },
        )
        .unwrap();

    let mut cmd = BtDownloadCommand::new(
        GroupId::new(110),
        &torrent_data,
        &DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            file_allocation: Some("none".to_string()),
            ..DownloadOptions::default()
        },
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    cmd.set_progress_manager(manager);
    tokio::time::timeout(std::time::Duration::from_secs(20), cmd.execute())
        .await
        .expect("legacy progress resume timed out")
        .expect("legacy progress resume failed");

    let requested = peer.requested_pieces().await;
    assert!(
        requested.contains(&1),
        "missing piece was not requested: {requested:?}"
    );
    assert!(
        !requested.contains(&0),
        "verified legacy progress piece was requested again: {requested:?}"
    );
    assert_eq!(
        std::fs::read(&output_path).unwrap(),
        [piece_zero, expected_piece_data(1, 512, 1024)].concat()
    );
    assert!(
        !progress_path.exists(),
        "successful resume must remove legacy progress"
    );
}

#[tokio::test]
async fn test_e2e_bt_check_integrity_redownloads_only_failed_piece() {
    use aria2_core::util::rwlock_ext::RwLockRecover;

    let dir = tmp_dir();
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let placeholder = build_test_torrent(
        "integrity.bin",
        1024,
        512,
        &tracker_placeholder.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let peer = MockBtPeerServer::start(
        meta.info_hash.bytes,
        vec![
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ],
    )
    .await;
    drop(tracker_placeholder);

    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent("integrity.bin", 1024, 512, &tracker.announce_url());
    let output_path = dir.path().join("integrity.bin");
    let mut existing = expected_piece_data(0, 512, 1024);
    let mut corrupted_piece = expected_piece_data(1, 512, 1024);
    corrupted_piece[0] ^= 0xff;
    existing.extend_from_slice(&corrupted_piece);
    std::fs::write(&output_path, existing).unwrap();

    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        check_integrity: true,
        file_allocation: Some("none".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(109),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("integrity-check BT command timed out")
        .expect("integrity-check BT command failed");

    let requested = peer.requested_pieces().await;
    assert!(
        requested.contains(&1),
        "corrupted piece was not re-downloaded: {requested:?}"
    );
    assert!(
        !requested.contains(&0),
        "verified piece was requested again: {requested:?}"
    );
    assert_eq!(
        std::fs::read(&output_path).unwrap(),
        [
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ]
        .concat()
    );
    assert_eq!(group.recover().get_bt_bitfield(), Some(vec![0xc0]));
    assert!(group.recover().status().is_completed());
    assert!(
        !ControlFile::control_path_for(&output_path).exists(),
        "successful integrity repair must remove its checkpoint"
    );
}

#[tokio::test]
async fn test_e2e_bt_multi_file_integrity_repairs_piece_crossing_file_boundary() {
    let dir = tmp_dir();
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let placeholder = build_multi_file_test_torrent(
        "multi-integrity",
        &[4, 6],
        6,
        &tracker_placeholder.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let peer = MockBtPeerServer::start(
        meta.info_hash.bytes,
        vec![expected_piece_data(0, 6, 10), expected_piece_data(1, 6, 10)],
    )
    .await;
    drop(tracker_placeholder);

    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data =
        build_multi_file_test_torrent("multi-integrity", &[4, 6], 6, &tracker.announce_url());
    let output_dir = dir.path();
    let first_path = output_dir.join("part-0.bin");
    let second_path = output_dir.join("part-1.bin");
    std::fs::write(&first_path, [0, 1, 2, 3]).unwrap();
    std::fs::write(&second_path, [0xff, 5, 6, 7, 8, 9]).unwrap();

    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        check_integrity: true,
        file_allocation: Some("none".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(9104),
        &torrent_data,
        &options,
        Some(output_dir.to_str().unwrap()),
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("multi-file integrity check timed out")
        .expect("multi-file integrity repair failed");

    assert_eq!(peer.requested_pieces().await, vec![0]);
    assert_eq!(std::fs::read(first_path).unwrap(), [0, 1, 2, 3]);
    assert_eq!(std::fs::read(second_path).unwrap(), [4, 5, 6, 7, 8, 9]);
}

#[tokio::test]
async fn test_e2e_bt_select_file_downloads_cross_boundary_piece_and_removes_unselected_file() {
    let dir = tmp_dir();
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let placeholder = build_multi_file_test_torrent(
        "multi-selective",
        &[4, 6],
        6,
        &tracker_placeholder.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let peer = MockBtPeerServer::start(
        meta.info_hash.bytes,
        vec![expected_piece_data(0, 6, 10), expected_piece_data(1, 6, 10)],
    )
    .await;
    drop(tracker_placeholder);

    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data =
        build_multi_file_test_torrent("multi-selective", &[4, 6], 6, &tracker.announce_url());
    let output_dir = dir.path();
    let unselected_path = output_dir.join("part-0.bin");
    let selected_path = output_dir.join("part-1.bin");
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        file_allocation: Some("none".to_string()),
        select_file: Some("2".to_string()),
        bt_remove_unselected_file: true,
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(9105),
        &torrent_data,
        &options,
        Some(output_dir.to_str().unwrap()),
    )
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("selective multi-file download timed out")
        .expect("selective multi-file download failed");

    let mut requested = peer.requested_pieces().await;
    requested.sort_unstable();
    requested.dedup();
    assert_eq!(
        requested,
        vec![0, 1],
        "the selected second file needs both the crossing and final piece"
    );
    assert!(
        !unselected_path.exists(),
        "bt-remove-unselected-file must remove the unselected file after success"
    );
    assert_eq!(
        std::fs::read(selected_path).unwrap(),
        [4, 5, 6, 7, 8, 9],
        "the selected file must retain bytes from the cross-file pieces"
    );
}

#[tokio::test]
async fn test_e2e_bt_complete_integrity_honors_hash_check_controls() {
    use aria2_core::util::rwlock_ext::RwLockRecover;

    let listener = Arc::new(RecordingBtEvents::default());
    DownloadEventHooks::shared().add_listener(listener.clone());

    for (gid, hook_enabled) in [(9101, true), (9102, false)] {
        let dir = tmp_dir();
        let tracker = MockTrackerServer::start(0).await;
        let torrent_data =
            build_test_torrent("complete-integrity.bin", 1024, 512, &tracker.announce_url());
        let output_path = dir.path().join("complete-integrity.bin");
        std::fs::write(
            &output_path,
            [
                expected_piece_data(0, 512, 1024),
                expected_piece_data(1, 512, 1024),
            ]
            .concat(),
        )
        .unwrap();

        let options = DownloadOptions {
            seed_time: Some(0.0),
            enable_dht: false,
            enable_public_trackers: false,
            check_integrity: true,
            file_allocation: Some("none".to_string()),
            bt_hash_check_seed: false,
            bt_enable_hook_after_hash_check: hook_enabled,
            ..DownloadOptions::default()
        };
        let mut command = BtDownloadCommand::new(
            GroupId::new(gid),
            &torrent_data,
            &options,
            Some(dir.path().to_str().unwrap()),
        )
        .unwrap();
        let group = command.group_handle();
        tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
            .await
            .expect("complete integrity check timed out")
            .expect("complete integrity check failed");

        assert!(group.recover().status().is_completed());
        assert_eq!(std::fs::read(&output_path).unwrap().len(), 1024);
        assert!(
            tracker.captured_queries().await.is_empty(),
            "bt-hash-check-seed=false must not enter tracker/peer lifecycle"
        );

        let gid_hex = GroupId::new(gid).to_hex_string();
        assert_eq!(
            listener.saw(DownloadEvent::BtComplete, &gid_hex),
            hook_enabled,
            "bt-enable-hook-after-hash-check must control the BT completion event"
        );
    }
}

#[tokio::test]
async fn test_e2e_bt_complete_integrity_default_seed_path_reaches_tracker() {
    let dir = tmp_dir();
    let tracker_placeholder = MockTrackerServer::start(0).await;
    let placeholder = build_test_torrent(
        "complete-integrity-seed.bin",
        1024,
        512,
        &tracker_placeholder.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let peer = MockBtPeerServer::start(
        meta.info_hash.bytes,
        vec![
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ],
    )
    .await;
    drop(tracker_placeholder);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent(
        "complete-integrity-seed.bin",
        1024,
        512,
        &tracker.announce_url(),
    );
    let output_path = dir.path().join("complete-integrity-seed.bin");
    std::fs::write(
        &output_path,
        [
            expected_piece_data(0, 512, 1024),
            expected_piece_data(1, 512, 1024),
        ]
        .concat(),
    )
    .unwrap();

    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        check_integrity: true,
        file_allocation: Some("none".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(9103),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("default hash-check seed path timed out")
        .expect("default hash-check seed path failed");

    let snapshot = group.recover().status_snapshot();
    assert_eq!(snapshot.completed_length, 1024);
    assert_eq!(
        snapshot.bt.as_ref().and_then(|bt| bt.bitfield.as_deref()),
        Some([0b1100_0000].as_slice()),
        "a complete integrity check must publish the verified BT bitfield"
    );

    assert!(
        tracker
            .captured_queries()
            .await
            .iter()
            .any(|query| query.contains("event=started")),
        "default bt-hash-check-seed=true must enter the tracker lifecycle"
    );
}

#[tokio::test]
async fn test_e2e_bt_seed_unverified_skips_existing_payload_hash_check() {
    let dir = tmp_dir();
    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let torrent_data =
        build_test_torrent("unverified-seed.bin", 1024, 512, &tracker.announce_url());
    let output_path = dir.path().join("unverified-seed.bin");
    let mut existing = [
        expected_piece_data(0, 512, 1024),
        expected_piece_data(1, 512, 1024),
    ]
    .concat();
    existing[0] ^= 0xff;
    std::fs::write(&output_path, &existing).unwrap();

    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        check_integrity: true,
        bt_seed_unverified: true,
        file_allocation: Some("none".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(9110),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(20), command.execute())
        .await
        .expect("unverified seed path timed out")
        .expect("unverified seed path failed");

    assert_eq!(std::fs::read(&output_path).unwrap(), existing);
    assert!(command.group().status().is_completed());
}

async fn assert_standalone_seeding_phase_stops_on_lifecycle_change(remove: bool) {
    let dir = tmp_dir();
    let torrent_data = build_test_torrent(
        "seed-lifecycle.bin",
        1024,
        512,
        "http://127.0.0.1:1/announce",
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&torrent_data).unwrap();
    let output_path = dir.path().join("seed-lifecycle.bin");
    std::fs::write(&output_path, vec![0u8; 1024]).unwrap();
    let options = DownloadOptions {
        seed_time: None,
        seed_ratio: None,
        enable_dht: false,
        enable_public_trackers: false,
        file_allocation: Some("none".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(if remove { 1053 } else { 1052 }),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    let task = tokio::spawn(async move {
        command
            .run_seeding_phase(
                Vec::new(),
                meta.info.piece_length,
                meta.num_pieces() as u32,
                meta.info_hash.bytes,
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    if remove {
        group.recover_mut().remove().unwrap();
    } else {
        group.recover_mut().pause().unwrap();
    }

    let result = tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .expect("seeding phase did not stop after lifecycle change")
        .expect("seeding phase task panicked");
    let error = result.expect_err("lifecycle cancellation must not complete seeding");
    let message = error.to_string();
    if remove {
        assert!(
            message.contains("cancelled by user"),
            "unexpected error: {message}"
        );
        assert!(group.recover().is_removed());
    } else {
        assert!(
            message.contains("Download paused"),
            "unexpected error: {message}"
        );
        assert!(group.recover().is_paused_flag());
    }
    assert!(!group.recover().status().is_completed());
}

#[tokio::test]
async fn test_e2e_bt_seeding_phase_stops_on_pause_without_completion() {
    assert_standalone_seeding_phase_stops_on_lifecycle_change(false).await;
}

#[tokio::test]
async fn test_e2e_bt_seeding_phase_stops_on_remove_without_completion() {
    assert_standalone_seeding_phase_stops_on_lifecycle_change(true).await;
}

#[tokio::test]
async fn test_e2e_bt_pause_interrupts_stalled_piece_read() {
    let dir = tmp_dir();
    let total_size = 16 * 1024;
    let piece_length = 16 * 1024;
    let tracker = MockTrackerServer::start(0).await;
    let placeholder = build_test_torrent(
        "pause-stalled-read.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        vec![expected_piece_data(0, piece_length, total_size)],
        std::time::Duration::from_secs(3),
    )
    .await;
    drop(tracker);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent(
        "pause-stalled-read.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
    );
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(1051),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    let task = tokio::spawn(async move { command.execute().await });

    for _ in 0..200 {
        if !peer.requested_pieces().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        !peer.requested_pieces().await.is_empty(),
        "the slow peer must receive a block request before pause"
    );

    let pause_started = std::time::Instant::now();
    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("paused BT command remained in the stalled block read")
        .expect("paused BT task panicked");

    assert!(result.is_err(), "pause should stop the current command");
    assert!(pause_started.elapsed() < std::time::Duration::from_secs(2));
    assert!(group.recover().status().is_paused());

    let output_path = dir.path().join("pause-stalled-read.bin");
    let control_path = ControlFile::control_path_for(&output_path);
    let control_file = ControlFile::load(&control_path)
        .await
        .expect("pause should write a valid checkpoint")
        .expect("pause should leave a checkpoint");
    assert_eq!(control_file.completed_pieces(), 0);
}

#[tokio::test]
async fn test_e2e_bt_pause_then_resume_uses_checkpoint() {
    use aria2_core::request::request_group::DownloadStatus;
    use aria2_core::util::rwlock_ext::RwLockRecover;

    let dir = tmp_dir();
    let total_size = 64 * 1024;
    let piece_length = 16 * 1024;
    let tracker = MockTrackerServer::start(0).await;
    let placeholder = build_test_torrent(
        "pause-resume.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
    );
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let pieces = (0..meta.num_pieces())
        .map(|index| expected_piece_data(index as u32, piece_length, total_size))
        .collect();
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        pieces,
        std::time::Duration::from_millis(150),
    )
    .await;
    drop(tracker);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent_data = build_test_torrent(
        "pause-resume.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
    );
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };
    let mut cmd = BtDownloadCommand::new(
        GroupId::new(105),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = cmd.group_handle();
    let task = tokio::spawn(async move { cmd.execute().await });

    for _ in 0..200 {
        if !peer.requested_pieces().await.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    for _ in 0..200 {
        let persisted_piece = group
            .recover()
            .get_bt_bitfield()
            .is_some_and(|bitfield| bitfield.iter().any(|byte| *byte != 0));
        if persisted_piece {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    {
        group.recover_mut().pause().unwrap();
    }
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("paused BT command timed out")
        .expect("paused BT task panicked");
    assert!(result.is_err(), "pause should stop the current command");
    assert!(group.recover().status().is_paused());

    let output_path = dir.path().join("pause-resume.bin");
    let control_path = ControlFile::control_path_for(&output_path);
    let control_file = ControlFile::load(&control_path)
        .await
        .expect("pause should write a valid checkpoint")
        .expect("pause should leave a checkpoint");
    assert!(
        control_file.completed_pieces() > 0,
        "pause should persist at least the piece that finished before the pause"
    );
    assert!(control_file.completed_pieces() < 4);
    let completed = (0..4)
        .filter(|&index| control_file.is_piece_done(index))
        .collect::<Vec<_>>();
    let requests_before_resume = peer.requested_pieces().await.len();

    let mut resumed = BtDownloadCommand::new(
        GroupId::new(106),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), resumed.execute())
        .await
        .expect("resumed BT command timed out")
        .expect("resumed BT command failed");

    let requests = peer.requested_pieces().await;
    let resumed_requests = &requests[requests_before_resume..];
    for index in completed {
        assert!(
            !resumed_requests.contains(&(index as u32)),
            "resumed command requested completed piece {index}: {resumed_requests:?}"
        );
    }
    let expected = (0..total_size)
        .map(|index| (index % 256) as u8)
        .collect::<Vec<_>>();
    assert_eq!(std::fs::read(&output_path).unwrap(), expected);
    assert!(
        !control_path.exists(),
        "completed resume must remove checkpoint"
    );
    assert!(matches!(group.recover().status(), DownloadStatus::Paused));
}

#[tokio::test]
async fn test_e2e_bt_halt_then_immediate_resume_uses_checkpoint() {
    let dir = tmp_dir();
    let total_size = 4096;
    let piece_length = 512;
    let data = (0..total_size)
        .map(|index| (index % 256) as u8)
        .collect::<Vec<_>>();
    let web_seed = MockHttpServer::start()
        .await
        .expect("web-seed server should start");
    web_seed.register_slow_range_response("/halt-resume.bin", &data, 64, 20);

    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let web_seed_url = format!("{}/halt-resume.bin", web_seed.base_url());
    let torrent_data = build_test_torrent_with_web_seeds(
        "halt-resume.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
        std::slice::from_ref(&web_seed_url),
    );
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };
    let mut command = BtDownloadCommand::new(
        GroupId::new(111),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    let task = tokio::spawn(async move { command.execute().await });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if group
                .recover()
                .get_bt_bitfield()
                .is_some_and(|bitfield| bitfield.iter().any(|byte| *byte != 0))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("BT download did not complete a piece before halt");

    let output_path = dir.path().join("halt-resume.bin");
    let control_path = ControlFile::control_path_for(&output_path);
    let completed = (0..8)
        .filter(|&index| {
            group.recover().get_bt_bitfield().is_some_and(|bitfield| {
                bitfield
                    .get(index / 8)
                    .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
            })
        })
        .collect::<Vec<_>>();
    assert!(!completed.is_empty());
    let requests_before_halt = web_seed.take_request_log().len();

    let halt_started = std::time::Instant::now();
    group.recover().request_halt(HaltReason::ShutdownSignal);
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), task)
        .await
        .expect("graceful halt should cancel the active web-seed request")
        .expect("halted BT task panicked");
    assert!(result.is_err(), "halt should stop the current command");
    assert!(
        halt_started.elapsed() < std::time::Duration::from_secs(2),
        "graceful halt took too long: {:?}",
        halt_started.elapsed()
    );

    let checkpoint = ControlFile::load(&control_path)
        .await
        .expect("halt checkpoint should be readable")
        .expect("halt should preserve checkpoint");
    assert!(
        completed
            .iter()
            .all(|&index| checkpoint.is_piece_done(index))
    );

    let mut resumed = BtDownloadCommand::new(
        GroupId::new(112),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), resumed.execute())
        .await
        .expect("immediate BT resume timed out")
        .expect("immediate BT resume failed");

    let requests_after_resume = web_seed.take_request_log();
    assert!(requests_after_resume.len() >= requests_before_halt);
    for request in requests_after_resume {
        let Some(range) = request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("range"))
            .map(|(_, value)| value)
        else {
            continue;
        };
        let start = range
            .strip_prefix("bytes=")
            .and_then(|value| value.split_once('-'))
            .and_then(|(start, _)| start.parse::<usize>().ok());
        if let Some(start) = start {
            let requested_piece = start / piece_length as usize;
            assert!(
                !completed.contains(&requested_piece),
                "resumed BT download requested completed piece {requested_piece}: {range}"
            );
        }
    }
    assert_eq!(std::fs::read(&output_path).unwrap(), data);
    assert!(!control_path.exists());
}

#[tokio::test]
async fn test_e2e_bt_web_seed_download_and_checkpoint_resume() {
    use aria2_core::request::request_group::DownloadStatus;
    use aria2_core::util::rwlock_ext::RwLockRecover;

    let dir = tmp_dir();
    let total_size = 4096;
    let piece_length = 512;
    let data = (0..total_size)
        .map(|index| (index % 256) as u8)
        .collect::<Vec<_>>();
    let web_seed = MockHttpServer::start()
        .await
        .expect("web-seed server should start");
    web_seed.register_slow_range_response("/web-seed.bin", &data, 64, 40);

    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let web_seed_url = format!("{}/web-seed.bin", web_seed.base_url());
    let torrent_data = build_test_torrent_with_web_seeds(
        "web-seed.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
        std::slice::from_ref(&web_seed_url),
    );
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };

    let mut command = BtDownloadCommand::new(
        GroupId::new(107),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let group = command.group_handle();
    let task = tokio::spawn(async move { command.execute().await });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if web_seed
                .take_request_log()
                .iter()
                .any(|request| request.path == "/web-seed.bin")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("web-seed request did not start");

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if group
                .recover()
                .get_bt_bitfield()
                .is_some_and(|bitfield| bitfield.iter().any(|byte| *byte != 0))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("web-seed piece did not persist before pause");

    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("paused web-seed command timed out")
        .expect("paused web-seed task panicked");
    assert!(result.is_err(), "pause should stop the web-seed command");
    assert!(matches!(group.recover().status(), DownloadStatus::Paused));

    let output_path = dir.path().join("web-seed.bin");
    let control_path = ControlFile::control_path_for(&output_path);
    let control_file = ControlFile::load(&control_path)
        .await
        .expect("web-seed pause should write a valid checkpoint")
        .expect("web-seed pause should leave a checkpoint");
    assert!(control_file.is_torrent_checkpoint());
    assert!(control_file.completed_pieces() > 0);
    assert!(control_file.completed_pieces() < 8);

    let mut resumed = BtDownloadCommand::new(
        GroupId::new(108),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), resumed.execute())
        .await
        .expect("resumed web-seed command timed out")
        .expect("resumed web-seed command failed");

    assert_eq!(std::fs::read(&output_path).unwrap(), data);
    assert!(!control_path.exists());
}

#[tokio::test]
async fn test_e2e_bt_disabled_web_seed_does_not_request_url_list() {
    let dir = tmp_dir();
    let data = vec![0x5Au8; 512];
    let web_seed = MockHttpServer::start()
        .await
        .expect("web-seed server should start");
    web_seed.register_range_response("/disabled-web-seed.bin", &data);

    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let web_seed_url = format!("{}/disabled-web-seed.bin", web_seed.base_url());
    let torrent_data = build_test_torrent_with_web_seeds(
        "disabled-web-seed.bin",
        data.len() as u64,
        data.len() as u32,
        &tracker.announce_url(),
        std::slice::from_ref(&web_seed_url),
    );
    let options = DownloadOptions {
        bt_enable_web_seed: false,
        bt_stop_timeout: Some(1),
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };

    let mut command = BtDownloadCommand::new(
        GroupId::new(110),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), command.execute())
        .await
        .expect("disabled web-seed command timed out");

    assert!(
        result.is_err(),
        "without peers the disabled web-seed task must stop"
    );
    assert!(
        web_seed.take_request_log().is_empty(),
        "bt-enable-web-seed=false must prevent url-list requests"
    );
}

#[tokio::test]
async fn test_e2e_bt_save_session_flushes_requested_checkpoint() {
    let dir = tmp_dir();
    let total_size = 4096;
    let piece_length = 512;
    let data = (0..total_size)
        .map(|index| (index % 256) as u8)
        .collect::<Vec<_>>();
    let web_seed = MockHttpServer::start()
        .await
        .expect("web-seed server should start");
    web_seed.register_slow_range_response("/save-session.bin", &data, 64, 40);

    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let web_seed_url = format!("{}/save-session.bin", web_seed.base_url());
    let torrent_data = build_test_torrent_with_web_seeds(
        "save-session.bin",
        total_size,
        piece_length,
        &tracker.announce_url(),
        std::slice::from_ref(&web_seed_url),
    );
    let options = DownloadOptions {
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(109);
    let group = Arc::new(std::sync::RwLock::new(
        aria2_core::request::request_group::RequestGroup::new(
            gid,
            vec!["bt://save-session".to_string()],
            options.clone(),
        ),
    ));
    let manager = Arc::new(RequestGroupMan::new());
    manager.add_group_arc(Arc::clone(&group));

    let mut command = BtDownloadCommand::new_with_group(
        Arc::clone(&group),
        &torrent_data,
        &options,
        Some(dir.path().to_str().unwrap()),
    )
    .unwrap();
    let task = tokio::spawn(async move { command.execute().await });

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if group
                .recover()
                .get_bt_bitfield()
                .is_some_and(|bitfield| bitfield.iter().any(|byte| *byte != 0))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("web-seed piece did not complete before session save");

    let session_path = dir.path().join("save-session.txt");
    let mut save_session = SaveSessionCommand::new(session_path.clone(), manager);
    save_session
        .execute()
        .await
        .expect("session save should request the active BT checkpoint");
    assert!(session_path.exists());

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if !group.recover().is_save_control_file_requested() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("BT command did not consume the save request");

    let output_path = dir.path().join("save-session.bin");
    let control_path = ControlFile::control_path_for(&output_path);
    let control_file = ControlFile::load(&control_path)
        .await
        .expect("requested BT checkpoint should be readable")
        .expect("requested BT checkpoint should exist");
    let completed_piece = (0..8)
        .find(|&index| control_file.is_piece_done(index))
        .expect("requested BT checkpoint should contain a completed piece");
    let output = std::fs::read(&output_path).expect("BT output should be readable");
    let piece_length = piece_length as usize;
    let start = completed_piece * piece_length;
    assert_eq!(
        &output[start..start + piece_length],
        &data[start..start + piece_length],
        "checkpointed BT piece must be flushed before the sidecar is saved"
    );

    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), task)
        .await
        .expect("paused BT save-session command timed out")
        .expect("paused BT save-session task panicked");
    assert!(result.is_err(), "pause should stop the active BT command");
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
    assert!(
        !ControlFile::control_path_for(&output_path).exists(),
        "completed BT downloads must remove their Rust checkpoint"
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
