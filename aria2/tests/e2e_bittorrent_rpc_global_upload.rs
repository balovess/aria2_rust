#![cfg(feature = "bittorrent")]

//! Process-wide BT upload-limit coverage across RPC-created torrents.

#[path = "support/mod.rs"]
mod support;

#[path = "support/bittorrent_upload.rs"]
mod upload_fixture;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use support::RunningAria2;
use upload_fixture::{
    DEFAULT_BURST_LENGTH, PIECE_LENGTH, PartialSeeder, UPLOAD_RATE_BYTES_PER_SEC,
    connect_interested_leecher, receive_piece, request_piece_blocks, reserve_loopback_port, rpc,
    upload_rate_torrent,
};

fn add_torrent(client: &RunningAria2, id: u64, torrent: Vec<u8>) -> String {
    rpc(
        client,
        id,
        "aria2.addTorrent",
        json!([
            base64::engine::general_purpose::STANDARD.encode(torrent),
            [],
            {}
        ]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned()
}

async fn wait_for_piece(client: &RunningAria2, gid: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let status = rpc(
                client,
                20,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["completedLength"].as_str() == Some(PIECE_LENGTH.to_string().as_str()) {
                assert_eq!(status["status"], "active");
                assert_eq!(
                    status["totalLength"].as_str(),
                    Some((PIECE_LENGTH * 2).to_string().as_str())
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("first verified piece must finish while each tail stays withheld");
}

async fn wait_for_complete(client: &RunningAria2, gid: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                client,
                21,
                "aria2.tellStatus",
                json!([gid, ["completedLength", "totalLength"]]),
            );
            if status["completedLength"] == status["totalLength"] {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("torrent tail must complete after the wire upload assertion");
}

async fn wait_for_uploaded_bytes(client: &RunningAria2, gid: &str) {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let status = rpc(
                client,
                22,
                "aria2.tellStatus",
                json!([gid, ["uploadLength"]]),
            );
            if status["uploadLength"].as_str() == Some(PIECE_LENGTH.to_string().as_str()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("RPC uploadLength must reflect the completed wire transfer");
}

#[tokio::test]
async fn cli_global_upload_limit_aggregates_rpc_added_torrents_on_peer_wire() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder_url = placeholder_tracker.announce_url();
    let placeholder_a = upload_rate_torrent(&placeholder_url, "global-upload-a.bin");
    let placeholder_b = upload_rate_torrent(&placeholder_url, "global-upload-b.bin");
    let meta_a = TorrentMeta::parse(&placeholder_a).expect("first torrent metadata parses");
    let meta_b = TorrentMeta::parse(&placeholder_b).expect("second torrent metadata parses");
    let payload = Arc::new([vec![0x41; PIECE_LENGTH], vec![0x42; PIECE_LENGTH]].concat());
    let peer_a = PartialSeeder::start(meta_a.info_hash.bytes, Arc::clone(&payload)).await;
    let peer_b = PartialSeeder::start(meta_b.info_hash.bytes, Arc::clone(&payload)).await;
    drop(placeholder_tracker);

    let tracker_a = MockTrackerServer::start(peer_a.addr.port()).await;
    let tracker_b = MockTrackerServer::start(peer_b.addr.port()).await;
    let torrent_a = upload_rate_torrent(&tracker_a.announce_url(), "global-upload-a.bin");
    let torrent_b = upload_rate_torrent(&tracker_b.announce_url(), "global-upload-b.bin");
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        format!(
            "--max-overall-upload-limit={}K",
            UPLOAD_RATE_BYTES_PER_SEC / 1024
        ),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=3600".to_owned(),
    ];

    // Start RPC-only: the configured process-wide limit must be installed
    // before RPC later creates the torrent commands.
    let client = RunningAria2::start_rpc(&args);
    let gid_a = add_torrent(&client, 1, torrent_a);
    let gid_b = add_torrent(&client, 2, torrent_b);
    wait_for_piece(&client, &gid_a).await;
    wait_for_piece(&client, &gid_b).await;

    let mut leecher_a = connect_interested_leecher(listen_port, meta_a.info_hash.bytes).await;
    let mut leecher_b = connect_interested_leecher(listen_port, meta_b.info_hash.bytes).await;
    let transfer_started = std::time::Instant::now();
    request_piece_blocks(&mut leecher_a, 0).await;
    request_piece_blocks(&mut leecher_b, 0).await;
    let (uploaded_a, uploaded_b) = tokio::time::timeout(Duration::from_secs(25), async {
        tokio::join!(
            receive_piece(&mut leecher_a, 0),
            receive_piece(&mut leecher_b, 0)
        )
    })
    .await
    .expect("both globally rate-limited uploads must finish");
    let elapsed = transfer_started.elapsed();

    assert_eq!(uploaded_a, payload[..PIECE_LENGTH]);
    assert_eq!(uploaded_b, payload[..PIECE_LENGTH]);
    let combined_bytes = PIECE_LENGTH * 2;
    let minimum_wait = Duration::from_millis(
        (((combined_bytes - DEFAULT_BURST_LENGTH) * 3 * 1000) / (UPLOAD_RATE_BYTES_PER_SEC * 4))
            as u64,
    );
    assert!(
        elapsed >= minimum_wait,
        "the combined torrent uploads should share the {} KiB/s process limit after its {} KiB burst; took {elapsed:?}, expected at least {minimum_wait:?}",
        UPLOAD_RATE_BYTES_PER_SEC / 1024,
        DEFAULT_BURST_LENGTH / 1024,
    );

    wait_for_uploaded_bytes(&client, &gid_a).await;
    wait_for_uploaded_bytes(&client, &gid_b).await;
    peer_a.release_tail();
    peer_b.release_tail();
    wait_for_complete(&client, &gid_a).await;
    wait_for_complete(&client, &gid_b).await;
    assert_eq!(
        std::fs::read(output_dir.path().join("global-upload-a.bin")).unwrap(),
        *payload
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("global-upload-b.bin")).unwrap(),
        *payload
    );
}
