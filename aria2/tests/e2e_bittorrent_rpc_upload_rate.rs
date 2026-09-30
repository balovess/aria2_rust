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
use upload_fixture::{
    DEFAULT_BURST_LENGTH, PIECE_LENGTH, PartialSeeder, UPLOAD_RATE_BYTES_PER_SEC, UPLOAD_RATE_KIB,
    connect_interested_leecher, receive_piece, request_piece_blocks, reserve_loopback_port, rpc,
    upload_rate_torrent,
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
            json!([gid, ["uploadLength"]]),
        );
        let uploaded = status["uploadLength"]
            .as_str()
            .and_then(|length| length.parse::<usize>().ok());
        if uploaded == Some(PIECE_LENGTH) {
            break;
        }
        assert!(
            Instant::now() < upload_deadline,
            "RPC uploadLength did not reflect the wire upload: {status}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
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
