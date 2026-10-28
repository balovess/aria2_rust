#![cfg(feature = "bittorrent")]

//! Starting the same torrent again in one engine: after it completed (for
//! example to fetch another of its files), and right after it was removed
//! (for example to restart it with new options).

mod e2e_helpers;
mod fixtures;

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::{EngineCommand, EngineCommandSender};
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use fixtures::mock_bt_peer::MockBtPeerServer;
use fixtures::mock_tracker::MockTrackerServer;
use fixtures::test_torrent_builder::{build_multi_file_test_torrent, expected_piece_data};
use std::sync::{Arc, RwLock};
use std::time::Duration;

type Group = Arc<RwLock<RequestGroup>>;

fn torrent_group(gid: u64, torrent: &[u8], dir: &std::path::Path, select_file: &str) -> Group {
    let options = DownloadOptions {
        dir: Some(dir.to_string_lossy().into_owned()),
        seed_time: Some(0.0),
        enable_dht: false,
        enable_public_trackers: false,
        file_allocation: Some("none".to_string()),
        select_file: Some(select_file.to_string()),
        ..DownloadOptions::default()
    };
    let group = Arc::new(RwLock::new(RequestGroup::new(
        GroupId::new(gid),
        vec![format!("bt://restart-{gid}")],
        options,
    )));
    group.recover().set_bt_metadata_data(torrent.to_vec());
    group
}

fn add(commands: &EngineCommandSender, group: &Group) {
    commands
        .send(EngineCommand::AddDownload {
            group: Arc::clone(group),
        })
        .expect("BT engine command channel should be open");
}

async fn wait_for(group: &Group, wanted: DownloadStatus, what: &str) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let status = group.recover().status();
            if status == wanted {
                return;
            }
            assert!(
                !matches!(status, DownloadStatus::Error(_)),
                "{what}: failed: {:?}",
                group.recover().get_last_error_message()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{what}: timed out in {:?}", group.recover().status()));
}

/// A two-file torrent (4 and 6 bytes, 6-byte pieces: the first piece
/// crosses into the second file), served by one mock peer through a mock
/// tracker, and an engine that keeps running between downloads.
async fn engine_and_torrent(
    name: &str,
    response_delay: Duration,
) -> (
    EngineCommandSender,
    Vec<u8>,
    MockBtPeerServer,
    MockTrackerServer,
) {
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder =
        build_multi_file_test_torrent(name, &[4, 6], 6, &placeholder_tracker.announce_url());
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder).unwrap();
    let pieces = vec![expected_piece_data(0, 6, 10), expected_piece_data(1, 6, 10)];
    let peer =
        MockBtPeerServer::start_with_response_delay(meta.info_hash.bytes, pieces, response_delay)
            .await;
    drop(placeholder_tracker);
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent = build_multi_file_test_torrent(name, &[4, 6], 6, &tracker.announce_url());

    let manager = Arc::new(RequestGroupMan::new());
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::clone(&manager));
    engine.set_keep_alive(true);
    let commands = engine.engine_command_sender();
    tokio::spawn(engine.run());
    (commands, torrent, peer, tracker)
}

#[tokio::test]
async fn test_e2e_engine_bt_same_torrent_restarts_after_completion() {
    let dir = tempfile::tempdir().unwrap();
    let (commands, torrent, _peer, _tracker) =
        engine_and_torrent("restart-done", Duration::ZERO).await;

    // The second file first.
    let first = torrent_group(9301, &torrent, dir.path(), "2");
    add(&commands, &first);
    wait_for(&first, DownloadStatus::Complete, "the first download").await;

    // Then the same torrent again, for the other file, into the same
    // folder: the finished task must not still hold the torrent's peer
    // route or its files' reservation.
    let second = torrent_group(9302, &torrent, dir.path(), "1");
    add(&commands, &second);
    wait_for(
        &second,
        DownloadStatus::Complete,
        "the same torrent started again",
    )
    .await;

    let torrent_dir = dir.path().join("restart-done");
    assert_eq!(
        std::fs::read(torrent_dir.join("part-0.bin")).unwrap(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        std::fs::read(torrent_dir.join("part-1.bin")).unwrap(),
        [4, 5, 6, 7, 8, 9]
    );
}

#[tokio::test]
async fn test_e2e_engine_bt_same_torrent_restarts_right_after_remove() {
    let dir = tempfile::tempdir().unwrap();
    // Slow answers keep the first download going until it's removed.
    let (commands, torrent, peer, _tracker) =
        engine_and_torrent("restart-removed", Duration::from_secs(2)).await;

    let first = torrent_group(9311, &torrent, dir.path(), "1-2");
    add(&commands, &first);
    wait_for(&first, DownloadStatus::Active, "the first download").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while peer.requested_pieces().await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the peer did not receive a piece request");

    // Removed, and started again at once (as a client does to apply new
    // options): the removed task is still shutting down, holding its route,
    // registration and files for a moment.
    commands
        .send(EngineCommand::RemoveDownload {
            gid: GroupId::new(9311),
        })
        .expect("remove should be accepted");
    let second = torrent_group(9312, &torrent, dir.path(), "1-2");
    add(&commands, &second);
    wait_for(&second, DownloadStatus::Complete, "the restarted download").await;

    let torrent_dir = dir.path().join("restart-removed");
    assert_eq!(
        std::fs::read(torrent_dir.join("part-0.bin")).unwrap(),
        [0, 1, 2, 3]
    );
    assert_eq!(
        std::fs::read(torrent_dir.join("part-1.bin")).unwrap(),
        [4, 5, 6, 7, 8, 9]
    );
}
