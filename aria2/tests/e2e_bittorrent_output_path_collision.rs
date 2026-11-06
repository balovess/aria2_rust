#![cfg(feature = "bittorrent")]

//! Process-level BitTorrent output-path compatibility regression coverage.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_core::filesystem::control_file::ControlFile;
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use support::RunningAria2;

fn rpc(client: &RunningAria2, id: u64, method: &str, params: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let response = client.post(
        "/jsonrpc",
        "application/json",
        request.to_string().as_bytes(),
    );
    assert_eq!(response.status, 200, "RPC HTTP status: {}", response.status);
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");
    assert!(response.get("error").is_none(), "RPC error: {response}");
    response["result"].clone()
}

fn torrent(tracker_url: &str, piece_length: i64) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(3));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"shared-output.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(piece_length));
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(vec![
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
        ]),
    );
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

fn add_torrent(client: &RunningAria2, id: u64, torrent: Vec<u8>) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(torrent);
    rpc(client, id, "aria2.addTorrent", json!([encoded, [], {}]))
        .as_str()
        .expect("addTorrent returns a GID")
        .to_owned()
}

fn remove_if_exists(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("failed to remove test output {}: {error}", path.display()),
    }
}

async fn wait_stopped(client: &RunningAria2, gid: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let stopped = rpc(client, 4, "aria2.tellStopped", json!([0, 100]));
        if let Some(result) = stopped
            .as_array()
            .and_then(|entries| entries.iter().find(|entry| entry["gid"] == gid))
        {
            return result.clone();
        }
        assert!(
            Instant::now() < deadline,
            "GID {gid} did not stop: {stopped}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_started_announces(tracker: &MockTrackerServer, expected: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let started = tracker
                .captured_queries()
                .await
                .iter()
                .filter(|query| query.contains("event=started"))
                .count();
            if started >= expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("tracker did not receive the expected started announce");
}

#[tokio::test]
async fn rpc_rejects_different_torrents_that_target_the_same_active_file() {
    let output_dir = tempfile::tempdir().expect("temporary output directory");
    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let args = [
        format!("--dir={}", output_dir.path().display()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--enable-lpd=false".to_owned(),
        "--enable-utp=false".to_owned(),
        "--bt-stop-timeout=300".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);

    let first_torrent = torrent(&tracker.announce_url(), 16 * 1024);
    let second_torrent = torrent(&tracker.announce_url(), 32 * 1024);
    let first_info_hash =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&first_torrent)
            .expect("first torrent parses")
            .info_hash;
    let second_info_hash =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&second_torrent)
            .expect("second torrent parses")
            .info_hash;
    assert_ne!(first_info_hash, second_info_hash);

    let first_gid = add_torrent(&client, 1, first_torrent);
    tracker.wait_for_event("started").await;
    let first_status = rpc(
        &client,
        2,
        "aria2.tellStatus",
        json!([first_gid, ["status", "completedLength"]]),
    );
    assert_eq!(first_status["status"], "active");
    assert_eq!(first_status["completedLength"], "0");
    let output_path = output_dir.path().join("shared-output.bin");
    let first_output = std::fs::read(&output_path).ok();

    let second_gid = add_torrent(&client, 3, second_torrent);
    let second_result = wait_stopped(&client, &second_gid).await;

    assert_eq!(second_result["status"], "error");
    assert_eq!(second_result["errorCode"], "11");
    assert_eq!(
        second_result["errorMessage"],
        format!(
            "File {} is being downloaded by other command.",
            output_dir.path().join("shared-output.bin").display()
        )
    );
    assert_eq!(
        tracker.captured_queries().await.len(),
        1,
        "a conflicting task must be rejected before tracker announce"
    );
    assert_eq!(
        std::fs::read(&output_path).ok(),
        first_output,
        "the rejected torrent must not create, truncate, or modify the first task's output"
    );
    let first_status = rpc(
        &client,
        5,
        "aria2.tellStatus",
        json!([first_gid, ["status", "completedLength"]]),
    );
    assert_eq!(first_status["status"], "active");
    assert_eq!(first_status["completedLength"], "0");

    let _ = rpc(&client, 6, "aria2.forceRemove", json!([first_gid]));
    let removed = wait_stopped(&client, &first_gid).await;
    assert_eq!(removed["status"], "removed");
    remove_if_exists(&output_path);
    remove_if_exists(&ControlFile::control_path_for(&output_path));

    let third_gid = add_torrent(&client, 7, torrent(&tracker.announce_url(), 64 * 1024));
    wait_started_announces(&tracker, 2).await;
    let third_status = rpc(
        &client,
        8,
        "aria2.tellStatus",
        json!([third_gid, ["status", "completedLength"]]),
    );
    assert_eq!(third_status["status"], "active");
    assert_eq!(third_status["completedLength"], "0");
}
