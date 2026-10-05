#![cfg(feature = "bittorrent")]

//! CLI/RPC regressions for BitTorrent peer lifecycle and mixed-source recovery.

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;
#[path = "support/bittorrent_peer_backpressure.rs"]
mod peer_backpressure;
#[path = "support/mod.rs"]
mod support;

use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use peer_backpressure::{
    BLOCK_LENGTH, FixturePeer, PeerMode, reserve_loopback_port, rpc, torrent_with_piece_length,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::sync::{Notify, Semaphore};

#[tokio::test]
async fn cli_rejects_last_peer_after_mixed_source_piece_hash_failure() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let blocks_per_piece = 7;
    // Stay above the normal-to-endgame threshold so the intended regular
    // request pipeline, not duplicate endgame responses, determines the hash.
    let piece_count = 22;
    let piece_length = blocks_per_piece * BLOCK_LENGTH;
    let payload = Arc::new(vec![0x5a; piece_count * piece_length]);
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder_torrent =
        torrent_with_piece_length(&placeholder_tracker.announce_url(), &payload, piece_length);
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder_torrent)
            .expect("generated torrent metadata parses");
    drop(placeholder_tracker);

    let corrupt_batch_ready = Arc::new(Notify::new());
    let release_corrupt_batch = Arc::new(Semaphore::new(0));
    let corrupt_peer = FixturePeer::start(
        meta.info_hash.bytes,
        [0xC3; 20],
        PeerMode::ServePiece {
            payload: Arc::clone(&payload),
            piece_count,
            corrupt: true,
            response_delay: Duration::ZERO,
            hold_initial_requests: blocks_per_piece - 1,
            held_batch_ready: Some(Arc::clone(&corrupt_batch_ready)),
            release_held_batch: Some(Arc::clone(&release_corrupt_batch)),
            wait_for_first_request: None,
        },
    )
    .await;
    let healthy_peer = FixturePeer::start(
        meta.info_hash.bytes,
        [0xD4; 20],
        PeerMode::ServePiece {
            payload: Arc::clone(&payload),
            piece_count,
            corrupt: false,
            response_delay: Duration::ZERO,
            hold_initial_requests: 0,
            held_batch_ready: None,
            release_held_batch: None,
            wait_for_first_request: Some(corrupt_batch_ready),
        },
    )
    .await;
    let tracker = MockTrackerServer::start_with_peers(
        vec![corrupt_peer.addr.port(), healthy_peer.addr.port()],
        false,
    )
    .await;
    let metainfo = torrent_with_piece_length(&tracker.announce_url(), &payload, piece_length);
    let listen_port = reserve_loopback_port();
    let log_path = output_dir.path().join("aria2.log");
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        format!("--log={}", log_path.display()),
        "--log-level=debug".to_owned(),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-max-peers=8".to_owned(),
        "--bt-request-timeout=2".to_owned(),
        "--max-tries=3".to_owned(),
    ];
    let mut client = RunningAria2::start_rpc(&args);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([
            base64::engine::general_purpose::STANDARD.encode(metainfo),
            [],
            {}
        ]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    tracker.wait_for_event("started").await;
    let expected_length = payload.len().to_string();
    let healthy_block_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let details = rpc(&client, 2, "aria2.getPeerDetails", json!([gid]));
        let healthy_contributed = details.as_array().is_some_and(|peers| {
            peers.iter().any(|peer| {
                peer["port"].as_u64() == Some(healthy_peer.addr.port() as u64)
                    && peer["downloadedBytes"]
                        .as_str()
                        .and_then(|bytes| bytes.parse::<usize>().ok())
                        .is_some_and(|bytes| bytes >= BLOCK_LENGTH)
            })
        });
        if healthy_contributed {
            break;
        }
        assert!(
            Instant::now() < healthy_block_deadline,
            "healthy peer did not contribute while the corrupt peer's initial requests were held: {details}; corrupt requests={}, healthy requests={}",
            corrupt_peer.block_requests_received.load(Ordering::SeqCst),
            healthy_peer.block_requests_received.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    release_corrupt_batch.add_permits(1);

    let download_deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength"]]),
        );
        if status["completedLength"].as_str() == Some(expected_length.as_str()) {
            break;
        }
        assert!(
            Instant::now() < download_deadline,
            "a mixed-source corrupt piece did not recover from its final contributing peer: {status}; corrupt requests={}, healthy requests={}",
            corrupt_peer.block_requests_received.load(Ordering::SeqCst),
            healthy_peer.block_requests_received.load(Ordering::SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        corrupt_peer.block_requests_received.load(Ordering::SeqCst) > 0,
        "the corrupt peer must contribute before the hash failure"
    );
    assert!(
        healthy_peer.block_requests_received.load(Ordering::SeqCst) > 0,
        "the healthy peer must contribute to the mixed-source first attempt"
    );
    let log = std::fs::read_to_string(&log_path).expect("read BT retry regression log");
    assert!(
        log.contains("SHA1 mismatch on piece 0"),
        "fixture must exercise a corrupt first attempt; log tail: {}",
        log.lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        log.contains("Rejected and removed peer after a piece hash mismatch"),
        "fixture must reject the final contributor after a mixed-source hash failure; corrupt={}, healthy={}, log tail: {}",
        corrupt_peer.block_requests_received.load(Ordering::SeqCst),
        healthy_peer.block_requests_received.load(Ordering::SeqCst),
        log.lines()
            .rev()
            .take(40)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    );

    rpc(&client, 3, "aria2.forceRemove", json!([gid]));
    rpc(&client, 4, "aria2.forceShutdown", json!([]));
    let exit = client.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "aria2c exits cleanly: {exit}");
}
