#![cfg(feature = "bittorrent")]

//! Process-level download-rate coverage through the RPC and peer-wire paths.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_bt_peer.rs"]
mod mock_bt_peer;
#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::{bencode::codec::BencodeValue, torrent::parser::TorrentMeta};
use base64::Engine as _;
use mock_bt_peer::MockBtPeerServer;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use support::RunningAria2;

const PIECE_LENGTH: usize = 32 * 1024;
const PIECE_SHA1: [u8; 20] = [
    0x51, 0x88, 0x43, 0x18, 0x49, 0xb4, 0x61, 0x31, 0x52, 0xfd, 0x7b, 0xdb, 0xa6, 0xa3, 0xff, 0x0a,
    0x4f, 0xd6, 0x42, 0x4b,
];

fn torrent_bytes(tracker_url: &str) -> Vec<u8> {
    torrent_bytes_with_piece_count(tracker_url, 1)
}

fn torrent_bytes_with_piece_count(tracker_url: &str, piece_count: usize) -> Vec<u8> {
    assert!(piece_count > 0);
    let mut info = BTreeMap::new();
    info.insert(
        b"length".to_vec(),
        BencodeValue::Int((PIECE_LENGTH * piece_count) as i64),
    );
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"rpc-download-rate.bin".to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(PIECE_LENGTH as i64),
    );
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(PIECE_SHA1.repeat(piece_count)),
    );

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

fn rpc(client: &RunningAria2, id: u64, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    let response = client.post(
        "/jsonrpc",
        "application/json",
        request.to_string().as_bytes(),
    );
    assert_eq!(
        response.status, 200,
        "RPC HTTP response: {:?}",
        response.headers
    );
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");
    assert!(response.get("error").is_none(), "RPC error: {response}");
    response["result"].clone()
}

fn status(client: &RunningAria2, gid: &str) -> Value {
    rpc(
        client,
        2,
        "aria2.tellStatus",
        json!([
            gid,
            [
                "status",
                "completedLength",
                "totalLength",
                "downloadSpeed",
                "uploadSpeed",
            ]
        ]),
    )
}

fn status_u64(status: &Value, field: &str) -> u64 {
    status[field]
        .as_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("tellStatus.{field} is not a u64: {status}"))
}

#[tokio::test]
async fn rpc_download_speed_tracks_recent_peer_payload_before_piece_verification() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = vec![0; PIECE_LENGTH];
    let placeholder = torrent_bytes("http://127.0.0.1:1/announce");
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        vec![payload.clone()],
        Duration::from_millis(1_500),
    )
    .await;
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent = torrent_bytes(&tracker.announce_url());
    let listen_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback peer port")
        .local_addr()
        .expect("read reserved peer port")
        .port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
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

    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = status(&client, &gid);
            if status_u64(&snapshot, "downloadSpeed") > 0 {
                assert_eq!(snapshot["completedLength"], "0");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("received peer payload must produce a live rate before the piece completes");

    // The peer waits 1.5 seconds before each block response. Check in the
    // intentional gap after one block has arrived but before the next one.
    tokio::time::sleep(Duration::from_millis(650)).await;
    let snapshot = status(&client, &gid);
    assert_eq!(snapshot["status"], "active");
    assert_eq!(snapshot["completedLength"], "0");
    assert!(
        status_u64(&snapshot, "downloadSpeed") > 0,
        "the recent payload rate must not collapse to zero while it is still inside aria2's 10-second speed window: {snapshot}"
    );

    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = status(&client, &gid);
            if status_u64(&snapshot, "completedLength") == PIECE_LENGTH as u64 {
                assert_eq!(snapshot["totalLength"], PIECE_LENGTH.to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect(
        "the piece must become visible only after both blocks arrive and verification succeeds",
    );

    assert_eq!(
        std::fs::read(output_dir.path().join("rpc-download-rate.bin"))
            .expect("downloaded file is readable"),
        payload,
    );
}

#[tokio::test]
async fn rpc_download_speed_stays_nonzero_during_sustained_peer_transfer() {
    const PIECE_COUNT: usize = 12;
    const RESPONSE_DELAY: Duration = Duration::from_millis(600);

    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = vec![0; PIECE_LENGTH];
    let piece_data = vec![payload.clone(); PIECE_COUNT];
    let placeholder = torrent_bytes_with_piece_count("http://127.0.0.1:1/announce", PIECE_COUNT);
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        piece_data,
        RESPONSE_DELAY,
    )
    .await;
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent = torrent_bytes_with_piece_count(&tracker.announce_url(), PIECE_COUNT);
    let listen_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback peer port")
        .local_addr()
        .expect("read reserved peer port")
        .port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
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

    let expected_length = PIECE_LENGTH * PIECE_COUNT;
    let mut first_nonzero_sample = None;
    let mut samples_after_payload = 0usize;
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            let snapshot = status(&client, &gid);
            let speed = status_u64(&snapshot, "downloadSpeed");
            let completed = status_u64(&snapshot, "completedLength");
            if speed > 0 {
                first_nonzero_sample.get_or_insert_with(std::time::Instant::now);
            }
            if first_nonzero_sample.is_some() {
                samples_after_payload += 1;
                assert_eq!(snapshot["status"], "active");
                assert!(
                    speed > 0,
                    "RPC speed became zero during a sustained peer transfer: {snapshot}"
                );
            }
            if completed == expected_length as u64 {
                assert_eq!(snapshot["totalLength"], expected_length.to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the controlled peer should finish the sustained transfer");

    let elapsed = first_nonzero_sample
        .expect("at least one peer payload must be observed")
        .elapsed();
    assert!(
        elapsed >= Duration::from_secs(10),
        "the scenario must cross aria2's full speed window; elapsed={elapsed:?}"
    );
    assert!(
        samples_after_payload >= 50,
        "sampled only {samples_after_payload} times"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("rpc-download-rate.bin"))
            .expect("downloaded file is readable"),
        payload.repeat(PIECE_COUNT),
    );
}

#[tokio::test]
async fn rpc_download_speed_expires_during_idle_seeding() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let log_path = output_dir.path().join("aria2.log");
    let payload = vec![0; PIECE_LENGTH];
    let placeholder = torrent_bytes("http://127.0.0.1:1/announce");
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        vec![payload],
        Duration::from_millis(1_500),
    )
    .await;
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent = torrent_bytes(&tracker.announce_url());
    let listen_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback peer port")
        .local_addr()
        .expect("read reserved peer port")
        .port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--log={}", log_path.display()),
        "--log-level=info".to_owned(),
        "--auto-save-interval=5".to_owned(),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
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

    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let snapshot = status(&client, &gid);
            if status_u64(&snapshot, "downloadSpeed") > 0 {
                assert_eq!(snapshot["completedLength"], "0");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the download must publish a nonzero rate before the torrent completes");

    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let snapshot = status(&client, &gid);
            if status_u64(&snapshot, "completedLength") == PIECE_LENGTH as u64 {
                assert_eq!(snapshot["status"], "active");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the torrent should finish downloading and remain in seeding mode");

    // aria2's speed window is ten seconds. With no further payload, the live
    // RPC rate must expire while the completed torrent remains active as a seed.
    tokio::time::sleep(Duration::from_secs(11)).await;
    let snapshot = status(&client, &gid);
    assert_eq!(snapshot["status"], "active");
    assert_eq!(snapshot["completedLength"], PIECE_LENGTH.to_string());
    assert_eq!(
        status_u64(&snapshot, "downloadSpeed"),
        0,
        "the last download sample must expire during idle seeding: {snapshot}"
    );
    let log = std::fs::read_to_string(&log_path).expect("read aria2 task log");
    let seeding_starts = log.matches("Seeding loop started").count();
    assert_eq!(
        seeding_starts, 1,
        "periodic checkpoint notifications must not restart the live seeding coordinator: {log}"
    );
}

#[tokio::test]
async fn tracker_discovered_peer_replaces_a_choked_peer_without_waiting_for_request_timeout() {
    const PIECE_COUNT: usize = 64;
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(12);

    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = vec![0; PIECE_LENGTH];
    let placeholder = torrent_bytes_with_piece_count("http://127.0.0.1:1/announce", PIECE_COUNT);
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let choked_peer = MockBtPeerServer::start_staying_choked(
        meta.info_hash.bytes,
        vec![payload.clone(); PIECE_COUNT],
    )
    .await;
    let healthy_peer =
        MockBtPeerServer::start(meta.info_hash.bytes, vec![payload.clone(); PIECE_COUNT]).await;
    let tracker = MockTrackerServer::start_with_event_peers(
        vec![choked_peer.addr().port()],
        vec![healthy_peer.addr().port()],
        1,
    )
    .await;
    let torrent = torrent_bytes_with_piece_count(&tracker.announce_url(), PIECE_COUNT);
    let listen_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback peer port")
        .local_addr()
        .expect("read reserved peer port")
        .port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        format!("--bt-request-timeout={}", REQUEST_TIMEOUT.as_secs()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
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

    assert!(
        choked_peer
            .wait_for_handshakes(1, Duration::from_secs(5))
            .await,
        "the initial tracker peer must connect and remain choked"
    );
    assert!(
        tracker
            .wait_for_query_count(2, Duration::from_secs(5))
            .await,
        "the tracker must announce the healthy replacement peer"
    );
    assert!(
        healthy_peer
            .wait_for_handshakes(1, Duration::from_secs(3))
            .await,
        "the replacement must be dialed before bt-request-timeout; queries={:?}, status={}",
        tracker.captured_queries().await,
        status(&client, &gid)
    );

    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let snapshot = status(&client, &gid);
            if status_u64(&snapshot, "completedLength") > 0 {
                assert!(
                    status_u64(&snapshot, "downloadSpeed") > 0,
                    "accepted payload should make the live RPC rate positive: {snapshot}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("a newly announced unchoked peer must deliver data before the old request timeout");

    assert!(
        !healthy_peer.requested_pieces().await.is_empty(),
        "the replacement peer must receive a real piece request"
    );
}

#[tokio::test]
async fn stopped_tell_status_reports_zero_transfer_speeds() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = vec![0; PIECE_LENGTH];
    let placeholder = torrent_bytes("http://127.0.0.1:1/announce");
    let meta = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let peer = MockBtPeerServer::start_with_response_delay(
        meta.info_hash.bytes,
        vec![payload.clone()],
        Duration::from_millis(1_500),
    )
    .await;
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let torrent = torrent_bytes(&tracker.announce_url());
    let listen_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback peer port")
        .local_addr()
        .expect("read reserved peer port")
        .port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--seed-time=0".to_owned(),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
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
            let snapshot = status(&client, &gid);
            if snapshot["status"] == "complete" {
                assert_eq!(snapshot["completedLength"], PIECE_LENGTH.to_string());
                assert_eq!(snapshot["downloadSpeed"], "0", "{snapshot}");
                assert_eq!(snapshot["uploadSpeed"], "0", "{snapshot}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("completed stopped task should report zero live transfer speeds");

    assert_eq!(
        std::fs::read(output_dir.path().join("rpc-download-rate.bin"))
            .expect("completed file is readable"),
        payload,
    );
}
