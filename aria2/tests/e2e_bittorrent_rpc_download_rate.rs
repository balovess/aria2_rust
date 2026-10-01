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
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(PIECE_LENGTH as i64));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"rpc-download-rate.bin".to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(PIECE_LENGTH as i64),
    );
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(PIECE_SHA1.to_vec()));

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
            ["status", "completedLength", "totalLength", "downloadSpeed"]
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
