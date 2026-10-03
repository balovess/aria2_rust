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
    BLOCK_LENGTH, FixturePeer, PIECE_LENGTH, PeerMode, reserve_loopback_port, rpc, torrent,
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use support::RunningAria2;

#[tokio::test]
async fn cli_isolates_a_backpressured_peer_and_force_removes_the_swarm() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = Arc::new(vec![0x5a; PIECE_LENGTH]);
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder_torrent = torrent(&placeholder_tracker.announce_url(), &payload);
    let meta =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder_torrent)
            .expect("generated torrent metadata parses");
    drop(placeholder_tracker);

    let stalled_peers = [
        FixturePeer::start(
            meta.info_hash.bytes,
            [0xA1; 20],
            PeerMode::StopReadingAfterRequestFlood,
        )
        .await,
        FixturePeer::start(
            meta.info_hash.bytes,
            [0xA2; 20],
            PeerMode::StopReadingAfterRequestFlood,
        )
        .await,
        FixturePeer::start(
            meta.info_hash.bytes,
            [0xA3; 20],
            PeerMode::StopReadingAfterRequestFlood,
        )
        .await,
    ];
    let mut idle_peers = Vec::with_capacity(16);
    for index in 0..16u8 {
        let mut peer_id = [0u8; 20];
        peer_id[0] = 0xC0 + index;
        idle_peers.push(FixturePeer::start(meta.info_hash.bytes, peer_id, PeerMode::Idle).await);
    }
    let healthy_peer =
        FixturePeer::start(meta.info_hash.bytes, [0xB2; 20], PeerMode::ReadOneUpload).await;
    let tracker = MockTrackerServer::start_with_peers(
        stalled_peers
            .iter()
            .map(|peer| peer.addr.port())
            .chain(idle_peers.iter().map(|peer| peer.addr.port()))
            .chain(std::iter::once(healthy_peer.addr.port()))
            .collect(),
        false,
    )
    .await;
    let metainfo = torrent(&tracker.announce_url(), &payload);
    std::fs::write(
        output_dir.path().join("backpressure.bin"),
        payload.as_slice(),
    )
    .expect("preseed the verified torrent payload");

    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-max-peers=32".to_owned(),
        "--bt-timeout=5".to_owned(),
        "--check-integrity=true".to_owned(),
        "--bt-hash-check-seed=true".to_owned(),
        "--seed-time=60".to_owned(),
        "--seed-ratio=0".to_owned(),
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

    let expected_length = PIECE_LENGTH.to_string();
    let ready_deadline = Instant::now() + Duration::from_secs(20);
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
            Instant::now() < ready_deadline,
            "preseed did not enter seeding: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracker.wait_for_event("started").await;

    let expected_ports = stalled_peers
        .iter()
        .chain(idle_peers.iter())
        .chain(std::iter::once(&healthy_peer))
        .map(|peer| u64::from(peer.addr.port()))
        .collect::<Vec<_>>();
    let admission_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let peers = rpc(&client, 3, "aria2.getPeerDetails", json!([gid]));
        if peers.as_array().is_some_and(|peers| {
            expected_ports.iter().all(|port| {
                peers
                    .iter()
                    .any(|peer| peer["port"].as_u64() == Some(*port))
            })
        }) {
            break;
        }
        assert!(
            Instant::now() < admission_deadline,
            "the seeder should admit all 20 peer actors before the backpressure check: {peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let requests_deadline = Instant::now() + Duration::from_secs(15);
    while stalled_peers
        .iter()
        .any(|peer| peer.requests_sent.load(Ordering::SeqCst) < PIECE_LENGTH / BLOCK_LENGTH)
    {
        assert!(
            Instant::now() < requests_deadline,
            "every stalled peer must flood its full request window: {:?}",
            stalled_peers
                .iter()
                .map(|peer| peer.requests_sent.load(Ordering::SeqCst))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let upload_deadline = Instant::now() + Duration::from_secs(10);
    while healthy_peer.uploaded_bytes.load(Ordering::SeqCst) < BLOCK_LENGTH {
        assert!(
            Instant::now() < upload_deadline,
            "backpressured peer prevented another actor from uploading"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let stalled_ports = stalled_peers
        .iter()
        .map(|peer| u64::from(peer.addr.port()))
        .collect::<Vec<_>>();
    let peer_details_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let details = rpc(&client, 4, "aria2.getPeerDetails", json!([gid]));
        let all_stalled_have_pending_uploads = details.as_array().is_some_and(|peers| {
            stalled_ports.iter().all(|port| {
                peers.iter().any(|peer| {
                    peer["port"].as_u64() == Some(*port)
                        && peer["outstandingRequestsFromPeer"]
                            .as_u64()
                            .is_some_and(|count| count > 0)
                })
            })
        });
        if all_stalled_have_pending_uploads {
            break;
        }
        assert!(
            Instant::now() < peer_details_deadline,
            "all three stalled peers must retain upload requests in flight before removal: {details}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let removal_started = Instant::now();
    rpc(&client, 5, "aria2.forceRemove", json!([gid]));
    assert!(
        removal_started.elapsed() < Duration::from_millis(2500),
        "forceRemove RPC duration grew with three backpressured peer actors: {:?}",
        removal_started.elapsed()
    );
    let removal_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let stopped = rpc(&client, 6, "aria2.tellStopped", json!([0, 100]));
        if stopped.as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry["gid"] == gid && entry["status"] == "removed")
        }) {
            break;
        }
        assert!(
            Instant::now() < removal_deadline,
            "forceRemove did not reach stopped state while three peers were backpressured: {stopped}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    rpc(&client, 7, "aria2.forceShutdown", json!([]));
    let exit = client.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "aria2c exits cleanly: {exit}");
}
