#![cfg(feature = "bittorrent")]

//! Process-level RPC-to-peer-wire upload-rate coverage.

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
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use support::RunningAria2;
use tokio::io::AsyncWriteExt;
use upload_fixture::{
    BLOCK_LENGTH, DEFAULT_BURST_LENGTH, PIECE_LENGTH, PartialSeeder, UPLOAD_RATE_BYTES_PER_SEC,
    UPLOAD_RATE_KIB, connect_interested_leecher, read_peer_message, receive_piece,
    request_piece_blocks, reserve_loopback_port, rpc, upload_rate_torrent,
};

#[tokio::test]
async fn cli_change_option_upload_limit_throttles_live_peer_actor_upload() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder =
        upload_rate_torrent(&placeholder_tracker.announce_url(), "rpc-upload-rate.bin");
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let payload = Arc::new([vec![0x41; PIECE_LENGTH], vec![0x42; PIECE_LENGTH]].concat());
    let peer = PartialSeeder::start(meta.info_hash.bytes, Arc::clone(&payload)).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = upload_rate_torrent(&tracker.announce_url(), "rpc-upload-rate.bin");
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=3600".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let torrent_base64 = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([torrent_base64, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            let completed = status["completedLength"]
                .as_str()
                .and_then(|length| length.parse::<usize>().ok());
            if completed == Some(PIECE_LENGTH) {
                assert_eq!(status["status"], "active");
                let total = status["totalLength"]
                    .as_str()
                    .and_then(|length| length.parse::<usize>().ok());
                assert_eq!(total, Some(PIECE_LENGTH * 2));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first verified piece must finish while the tail is withheld");

    assert_eq!(
        rpc(
            &client,
            3,
            "aria2.changeOption",
            json!([gid, {"max-upload-limit": format!("{UPLOAD_RATE_KIB}K")}]),
        ),
        "OK"
    );

    let mut leecher = connect_interested_leecher(listen_port, meta.info_hash.bytes).await;

    let transfer_started = Instant::now();
    request_piece_blocks(&mut leecher, 0).await;

    let received = tokio::time::timeout(Duration::from_secs(15), receive_piece(&mut leecher, 0))
        .await
        .expect("rate-limited upload did not finish within 15 seconds");
    let elapsed = transfer_started.elapsed();

    assert_eq!(&received, &payload[..PIECE_LENGTH]);
    let minimum_wait = Duration::from_millis(
        (((PIECE_LENGTH - DEFAULT_BURST_LENGTH) * 3 * 1000) / (UPLOAD_RATE_BYTES_PER_SEC * 4))
            as u64,
    );
    assert!(
        elapsed >= minimum_wait,
        "{} KiB/s after the default {} KiB burst should throttle this {} KiB piece by at least {minimum_wait:?}; transfer took {elapsed:?}",
        UPLOAD_RATE_KIB,
        DEFAULT_BURST_LENGTH / 1024,
        PIECE_LENGTH / 1024,
    );

    let upload_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let status = rpc(
            &client,
            4,
            "aria2.tellStatus",
            json!([gid, ["uploadLength", "uploadSpeed"]]),
        );
        let uploaded = status["uploadLength"]
            .as_str()
            .and_then(|length| length.parse::<usize>().ok());
        let upload_speed = status["uploadSpeed"]
            .as_str()
            .and_then(|speed| speed.parse::<u64>().ok());
        let peers = rpc(&client, 5, "aria2.getPeerDetails", json!([gid]));
        let peer_uploading = peers.as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["uploadSpeed"].as_u64().is_some_and(|speed| speed > 0))
        });
        if uploaded == Some(PIECE_LENGTH)
            && upload_speed.is_some_and(|speed| speed > 0)
            && peer_uploading
        {
            break;
        }
        assert!(
            Instant::now() < upload_deadline,
            "RPC must expose the active torrent and peer upload rates from the verified wire transfer: status={status}, peers={peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    peer.release_tail();
    let completion_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &client,
            5,
            "aria2.tellStatus",
            json!([gid, ["completedLength", "totalLength"]]),
        );
        if status["completedLength"] == status["totalLength"] {
            break;
        }
        assert!(
            Instant::now() < completion_deadline,
            "tail piece did not finish after the upload assertion: {status}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read(output_dir.path().join("rpc-upload-rate.bin")).unwrap(),
        *payload,
        "the complete file must match both verified source pieces"
    );
}

#[tokio::test]
async fn cli_uploads_verified_piece_from_small_write_back_cache_during_download() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder =
        upload_rate_torrent(&placeholder_tracker.announce_url(), "rpc-upload-cache.bin");
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let payload = Arc::new([vec![0x41; PIECE_LENGTH], vec![0x42; PIECE_LENGTH]].concat());
    let peer = PartialSeeder::start(meta.info_hash.bytes, Arc::clone(&payload)).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = upload_rate_torrent(&tracker.announce_url(), "rpc-upload-cache.bin");
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--disk-cache=4K".to_owned(),
        "--seed-time=3600".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let torrent_base64 = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([torrent_base64, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            let completed = status["completedLength"]
                .as_str()
                .and_then(|length| length.parse::<usize>().ok());
            if completed == Some(PIECE_LENGTH) {
                assert_eq!(status["status"], "active");
                let total = status["totalLength"]
                    .as_str()
                    .and_then(|length| length.parse::<usize>().ok());
                assert_eq!(total, Some(PIECE_LENGTH * 2));
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first Piece must be verified while the tail remains unavailable");

    let output_path = output_dir.path().join("rpc-upload-cache.bin");
    let persisted = std::fs::read(&output_path).expect("download output exists");
    assert_ne!(
        persisted.get(..PIECE_LENGTH),
        Some(&payload[..PIECE_LENGTH]),
        "the verified Piece must still be newer than disk so this exercises the live write-back cache"
    );

    let mut leecher = connect_interested_leecher(listen_port, meta.info_hash.bytes).await;
    let cross_boundary_offset = (BLOCK_LENGTH / 2) as u32;
    let mut cross_boundary_request = Vec::with_capacity(17);
    cross_boundary_request.extend_from_slice(&13u32.to_be_bytes());
    cross_boundary_request.push(6);
    cross_boundary_request.extend_from_slice(&0u32.to_be_bytes());
    cross_boundary_request.extend_from_slice(&cross_boundary_offset.to_be_bytes());
    cross_boundary_request.extend_from_slice(&(BLOCK_LENGTH as u32).to_be_bytes());
    leecher
        .write_all(&cross_boundary_request)
        .await
        .expect("request a range spanning adjacent cache entries");
    let cross_boundary_piece = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = read_peer_message(&mut leecher)
                .await
                .expect("read cross-cache upload response")
                .expect("peer disconnected before the cross-cache response");
            if message.first() == Some(&7) {
                break message;
            }
        }
    })
    .await
    .expect("cross-cache upload response timed out");
    assert_eq!(&cross_boundary_piece[1..5], &0u32.to_be_bytes());
    assert_eq!(
        u32::from_be_bytes(cross_boundary_piece[5..9].try_into().unwrap()),
        cross_boundary_offset
    );
    assert_eq!(
        &cross_boundary_piece[9..],
        &payload[cross_boundary_offset as usize..cross_boundary_offset as usize + BLOCK_LENGTH]
    );

    request_piece_blocks(&mut leecher, 0).await;
    let received = tokio::time::timeout(Duration::from_secs(10), receive_piece(&mut leecher, 0))
        .await
        .expect("small-cache upload did not return the verified Piece");
    assert_eq!(received.as_slice(), &payload[..PIECE_LENGTH]);

    let status = rpc(
        &client,
        3,
        "aria2.tellStatus",
        json!([gid, ["uploadLength", "uploadSpeed"]]),
    );
    assert_eq!(
        status["uploadLength"],
        (PIECE_LENGTH + BLOCK_LENGTH).to_string()
    );
    assert!(
        status["uploadSpeed"]
            .as_str()
            .and_then(|speed| speed.parse::<u64>().ok())
            .is_some_and(|speed| speed > 0),
        "the verified wire upload must be reflected in the active torrent rate: {status}"
    );

    peer.release_tail();
    let completion_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &client,
            4,
            "aria2.tellStatus",
            json!([gid, ["completedLength", "totalLength"]]),
        );
        if status["completedLength"] == status["totalLength"] {
            break;
        }
        assert!(
            Instant::now() < completion_deadline,
            "the tail Piece did not finish: {status}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read(output_path).unwrap(),
        *payload,
        "the completed file must preserve both source Pieces"
    );
}
