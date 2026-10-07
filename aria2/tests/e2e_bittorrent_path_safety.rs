#![cfg(feature = "bittorrent")]

#[path = "support/mod.rs"]
mod support;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::{self, JoinHandle};
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
        assert!(
            Instant::now() < deadline,
            "torrent task did not stop: {stopped}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(feature = "metalink")]
fn serve_http_body_once(body: Vec<u8>) -> (String, JoinHandle<String>) {
    serve_http_response_once("/payload.torrent", "application/x-bittorrent", body)
}

#[cfg(feature = "metalink")]
fn serve_http_response_once(
    path: &str,
    content_type: &str,
    body: Vec<u8>,
) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind metadata fixture");
    let address = listener.local_addr().expect("metadata fixture address");
    listener
        .set_nonblocking(true)
        .expect("set fixture listener nonblocking");
    let path = path.to_owned();
    let content_type = content_type.to_owned();
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let (mut stream, _) = match listener.accept() {
                Ok(connection) => connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "fixture timed out awaiting GET");
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(error) => panic!("accept metadata request: {error}"),
            };
            stream
                .set_nonblocking(false)
                .expect("set fixture stream blocking");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("set fixture read timeout");
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut chunk).expect("read metadata request");
                assert_ne!(read, 0, "aria2 closed an incomplete HTTP request");
                request.extend_from_slice(&chunk[..read]);
            }
            let request_line = std::str::from_utf8(&request)
                .expect("HTTP request must be UTF-8")
                .lines()
                .next()
                .expect("HTTP request line");
            let method = request_line
                .split_whitespace()
                .next()
                .expect("HTTP method")
                .to_owned();

            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len(),
            );
            stream
                .write_all(headers.as_bytes())
                .expect("write fixture response headers");
            if method != "HEAD" {
                stream
                    .write_all(&body)
                    .expect("write fixture response body");
                return method;
            }
        }
    });
    (format!("http://{address}{path}"), server)
}

#[cfg(feature = "metalink")]
async fn wait_for_stopped_gid(client: &RunningAria2, gid: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let stopped = rpc(client, 2, "aria2.tellStopped", json!([0, 100]));
        if let Some(task) = stopped
            .as_array()
            .and_then(|tasks| tasks.iter().find(|task| task["gid"] == gid))
        {
            return task.clone();
        }
        if Instant::now() >= deadline {
            let status = rpc(client, 3, "aria2.tellStatus", json!([gid]));
            let waiting = rpc(client, 4, "aria2.tellWaiting", json!([0, 100]));
            panic!(
                "Metalink payload {gid} did not stop: stopped={stopped}; status={status}; waiting={waiting}"
            );
        }
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

#[test]
fn rpc_add_torrent_parse_failure_uses_aria2_execution_error_code() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let client = RunningAria2::start_rpc(&[format!("--dir={}", temp.path().display())]);
    let encoded = base64::engine::general_purpose::STANDARD.encode(torrent_with_traversal_name());
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "aria2.addTorrent",
        "params": [encoded, [], {}],
    });
    let response = client.post(
        "/jsonrpc",
        "application/json",
        request.to_string().as_bytes(),
    );
    assert_eq!(response.status, 400, "RPC HTTP status: {}", response.status);
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");

    // aria2_original's RpcMethod::execute catches this input-time
    // RecoverableException and emits execution code 1; result code 26 is only
    // available when a download task exists.
    assert_eq!(response["error"]["code"], 1, "RPC response: {response}");
    assert!(response.get("result").is_none(), "RPC response: {response}");
}

#[cfg(feature = "metalink")]
#[tokio::test]
async fn rpc_metalink_bt_dependency_parse_failure_uses_result_code_26() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let downloads = temp.path().join("downloads");
    std::fs::create_dir(&downloads).expect("create download directory");
    let (metadata_url, server) = serve_http_body_once(torrent_with_traversal_name());
    let client = RunningAria2::start_rpc(&[
        format!("--dir={}", downloads.display()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--enable-lpd=false".to_owned(),
        "--enable-utp=false".to_owned(),
    ]);
    let metalink = format!(
        "<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"payload.bin\"><size>1</size><metaurl mediatype=\"torrent\">{metadata_url}</metaurl></file></metalink>"
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(metalink);
    let gids = rpc(&client, 1, "aria2.addMetalink", json!([encoded]));
    let gids: Vec<String> = serde_json::from_value(gids).expect("metadata and payload GIDs");
    assert_eq!(
        gids.len(),
        2,
        "RPC should publish the metadata/payload graph"
    );

    let failed_payload = wait_for_stopped_gid(&client, &gids[1]).await;
    server
        .join()
        .expect("metadata fixture thread should finish");

    assert_eq!(failed_payload["status"], "error");
    assert_eq!(failed_payload["errorCode"], "26");
    assert!(
        !temp.path().join("escaped.bin").exists(),
        "invalid metainfo must not create a path outside the output directory"
    );
}

#[cfg(feature = "metalink")]
#[tokio::test]
async fn rpc_metalink_bt_dependency_parse_failure_falls_back_to_direct_mirror() {
    let temp = tempfile::tempdir().expect("temporary test directory");
    let downloads = temp.path().join("downloads");
    std::fs::create_dir(&downloads).expect("create download directory");
    let expected = b"payload from the direct Metalink mirror".to_vec();
    let (metadata_url, metadata_server) = serve_http_body_once(torrent_with_traversal_name());
    let (fallback_url, fallback_server) = serve_http_response_once(
        "/fallback.bin",
        "application/octet-stream",
        expected.clone(),
    );
    let client = RunningAria2::start_rpc(&[
        format!("--dir={}", downloads.display()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--enable-lpd=false".to_owned(),
        "--enable-utp=false".to_owned(),
        "--follow-torrent=false".to_owned(),
    ]);
    let metalink = format!(
        "<metalink xmlns=\"urn:ietf:params:xml:ns:metalink\"><file name=\"payload.bin\"><size>{}</size><url priority=\"1\">{fallback_url}</url><metaurl mediatype=\"torrent\">{metadata_url}</metaurl></file></metalink>",
        expected.len()
    );
    let encoded = base64::engine::general_purpose::STANDARD.encode(metalink);
    let gids = rpc(&client, 1, "aria2.addMetalink", json!([encoded]));
    let gids: Vec<String> = serde_json::from_value(gids).expect("metadata and payload GIDs");
    assert_eq!(
        gids.len(),
        2,
        "RPC should publish the metadata/payload graph"
    );

    metadata_server
        .join()
        .expect("metadata fixture thread should finish");
    let fixture_deadline = Instant::now() + Duration::from_secs(5);
    while !fallback_server.is_finished() && Instant::now() < fixture_deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        fallback_server.is_finished(),
        "the direct mirror fixture was never requested"
    );
    let fallback_method = fallback_server
        .join()
        .expect("fallback fixture thread should finish");
    assert_eq!(fallback_method, "GET", "payload bytes require an HTTP GET");

    let payload_result = wait_for_stopped_gid(&client, &gids[1]).await;

    assert_eq!(payload_result["status"], "complete");
    assert_eq!(
        payload_result["completedLength"],
        expected.len().to_string()
    );
    assert_eq!(
        std::fs::read(downloads.join("payload.bin")).expect("read fallback output"),
        expected
    );
    assert!(
        !temp.path().join("escaped.bin").exists(),
        "invalid torrent metadata must not create a path outside the output directory"
    );
}
