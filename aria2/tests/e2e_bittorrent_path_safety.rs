#![cfg(feature = "bittorrent")]

#[path = "support/mod.rs"]
mod support;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use support::RunningAria2;

fn torrent_with_traversal_name() -> Vec<u8> {
    let info = BTreeMap::from([
        (
            b"name".to_vec(),
            BencodeValue::Bytes(b"../escaped.bin".to_vec()),
        ),
        (b"length".to_vec(), BencodeValue::Int(1)),
        (b"piece length".to_vec(), BencodeValue::Int(16_384)),
        (b"pieces".to_vec(), BencodeValue::Bytes(vec![0; 20])),
    ]);
    BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(b"http://127.0.0.1:1/announce".to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]))
    .encode()
}

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

async fn wait_for_stopped_task(client: &RunningAria2) -> Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let stopped = rpc(client, 1, "aria2.tellStopped", json!([0, 100]));
        if let Some(task) = stopped.as_array().and_then(|tasks| tasks.first()) {
            return task.clone();
        }
        assert!(Instant::now() < deadline, "torrent task did not stop: {stopped}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cli_torrent_path_failure_uses_bittorrent_parse_result_code() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let downloads = temp.path().join("downloads");
    std::fs::create_dir(&downloads).expect("create download directory");
    let torrent_path = temp.path().join("unsafe.torrent");
    std::fs::write(&torrent_path, torrent_with_traversal_name()).expect("write torrent fixture");

    let args = [
        format!("--dir={}", downloads.display()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--enable-lpd=false".to_owned(),
        "--enable-utp=false".to_owned(),
        torrent_path.to_string_lossy().into_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let stopped = wait_for_stopped_task(&client).await;

    assert_eq!(stopped["status"], "error");
    assert_eq!(stopped["errorCode"], "26");
    assert!(
        !temp.path().join("escaped.bin").exists(),
        "unsafe torrent path must not create an output outside --dir"
    );
}
