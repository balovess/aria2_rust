#![cfg(feature = "bittorrent")]

//! CLI/RPC coverage for multi-file WebSeed writes across checkpoint restart.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_core::checksum::message_digest::{HashType, MessageDigest};
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, JoinSet};

const PIECE_LENGTH: usize = 32 * 1024;
const FILE_LENGTHS: [usize; 3] = [8 * 1024, 8 * 1024, 48 * 1024];
const TORRENT_NAME: &str = "webseed-checkpoint";

#[derive(Clone, Debug, PartialEq, Eq)]
struct WebSeedRequest {
    path: String,
    range: String,
}

struct MultiFileWebSeed {
    address: SocketAddr,
    release_tail_piece: watch::Sender<bool>,
    requests: Arc<Mutex<Vec<WebSeedRequest>>>,
    task: JoinHandle<()>,
}

impl MultiFileWebSeed {
    async fn start(payload: Arc<Vec<u8>>) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind multi-file WebSeed fixture");
        let address = listener.local_addr().expect("WebSeed fixture address");
        let (release_tail_piece, _) = watch::channel(false);
        let requests = Arc::new(Mutex::new(Vec::new()));

        let release_for_task = release_tail_piece.clone();
        let requests_for_task = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        handlers.spawn(serve_web_seed_request(
                            stream,
                            Arc::clone(&payload),
                            Arc::clone(&requests_for_task),
                            release_for_task.subscribe(),
                        ));
                    }
                    Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
                }
            }
        });

        Self {
            address,
            release_tail_piece,
            requests,
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/webseed/", self.address)
    }

    fn release_tail(&self) {
        self.release_tail_piece.send_replace(true);
    }

    async fn requests(&self) -> Vec<WebSeedRequest> {
        self.requests.lock().await.clone()
    }

    async fn wait_for_request(&self, path: &str, range: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .requests()
                .await
                .iter()
                .any(|request| request.path == path && request.range == range)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "WebSeed did not receive {path} with Range {range}; requests={:?}",
                self.requests().await
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

impl Drop for MultiFileWebSeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_web_seed_request(
    mut stream: TcpStream,
    payload: Arc<Vec<u8>>,
    requests: Arc<Mutex<Vec<WebSeedRequest>>>,
    mut release_tail_piece: watch::Receiver<bool>,
) {
    let Some((path, range)) = read_request_head(&mut stream).await else {
        return;
    };
    requests.lock().await.push(WebSeedRequest {
        path: path.clone(),
        range: range.clone(),
    });

    let Some((file_index, file_offset)) = file_for_path(&path) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let Some((start, end_inclusive)) = parse_byte_range(&range) else {
        return;
    };

    let Some(file_length) = FILE_LENGTHS.get(file_index).copied() else {
        return;
    };
    let end_exclusive = end_inclusive.saturating_add(1);
    if start >= end_exclusive || end_exclusive > file_length {
        return;
    }
    let global_start = file_offset + start;
    let global_end = file_offset + end_exclusive;
    let Some(data) = payload.get(global_start..global_end) else {
        return;
    };
    let headers = format!(
        "HTTP/1.1 206 Partial Content\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
        data.len(),
        start,
        end_inclusive,
        file_length,
    );
    if stream.write_all(headers.as_bytes()).await.is_err() {
        return;
    }

    // Piece 1 occupies bytes 16 KiB..48 KiB of the third file. Return its
    // response headers promptly to release the WebSeed probe gate, but hold
    // the body so pause/checkpoint timing stays deterministic.
    if file_index == 2 && start >= PIECE_LENGTH - (FILE_LENGTHS[0] + FILE_LENGTHS[1]) {
        while !*release_tail_piece.borrow_and_update() {
            if release_tail_piece.changed().await.is_err() {
                return;
            }
        }
    }
    let _ = stream.write_all(data).await;
}

async fn read_request_head(stream: &mut TcpStream) -> Option<(String, String)> {
    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0u8; 1024];
    loop {
        let count = stream.read(&mut buffer).await.ok()?;
        if count == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() > 16 * 1024 {
            return None;
        }
    }

    let request = std::str::from_utf8(&request).ok()?;
    let path = request
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .to_owned();
    let range = request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("range")
            .then(|| value.trim().to_owned())
    })?;
    Some((path, range))
}

fn file_for_path(path: &str) -> Option<(usize, usize)> {
    let file_index = (0..FILE_LENGTHS.len())
        .find(|index| path.ends_with(&format!("{TORRENT_NAME}/part-{index}.bin")))?;
    let file_offset = FILE_LENGTHS[..file_index].iter().sum();
    Some((file_index, file_offset))
}

fn parse_byte_range(range: &str) -> Option<(usize, usize)> {
    let (start, end) = range.strip_prefix("bytes=")?.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
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
    assert_eq!(response.status, 200, "RPC HTTP response status");
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");
    assert!(response.get("error").is_none(), "RPC error: {response}");
    response["result"].clone()
}

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve loopback port")
        .local_addr()
        .expect("bound socket has an address")
        .port()
}

fn multi_file_web_seed_torrent(tracker_url: &str, web_seed_url: &str, payload: &[u8]) -> Vec<u8> {
    assert_eq!(payload.len(), PIECE_LENGTH * 2);
    let pieces = payload
        .chunks(PIECE_LENGTH)
        .flat_map(|piece| MessageDigest::hash_data(HashType::Sha1, piece))
        .collect();
    let files = FILE_LENGTHS
        .iter()
        .enumerate()
        .map(|(index, length)| {
            let mut file = BTreeMap::new();
            file.insert(b"length".to_vec(), BencodeValue::Int(*length as i64));
            file.insert(
                b"path".to_vec(),
                BencodeValue::List(vec![BencodeValue::Bytes(
                    format!("part-{index}.bin").into_bytes(),
                )]),
            );
            BencodeValue::Dict(file)
        })
        .collect();

    let mut info = BTreeMap::new();
    info.insert(b"files".to_vec(), BencodeValue::List(files));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(TORRENT_NAME.as_bytes().to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(PIECE_LENGTH as i64),
    );
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::Bytes(web_seed_url.as_bytes().to_vec()),
    );
    BencodeValue::Dict(root).encode()
}

async fn wait_for_field(
    client: &RunningAria2,
    gid: &str,
    field: &str,
    expected: &str,
    timeout: Duration,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let status = rpc(
            client,
            20,
            "aria2.tellStatus",
            json!([gid, ["status", field, "totalLength"]]),
        );
        if status[field] == expected {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "wanted {field}={expected}; last status={status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cli_resumes_multifile_web_seed_checkpoint_without_refetching_completed_piece() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = Arc::new(
        (0..FILE_LENGTHS.iter().sum())
            .map(|index| ((index * 31 + 7) % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let web_seed = MultiFileWebSeed::start(Arc::clone(&payload)).await;
    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let torrent =
        multi_file_web_seed_torrent(&tracker.announce_url(), &web_seed.base_url(), &payload);
    let session_path = output_dir.path().join("webseed-session.txt");
    let listen_port = reserve_loopback_port();
    let common_args = || {
        [
            format!("--dir={}", output_dir.path().display()),
            format!("--listen-port={listen_port}"),
            "--enable-dht=false".to_owned(),
            "--enable-public-trackers=false".to_owned(),
            "--enable-peer-exchange=false".to_owned(),
            "--bt-enable-web-seed=true".to_owned(),
            "--seed-time=3600".to_owned(),
            format!("--save-session={}", session_path.display()),
        ]
    };

    let mut first = RunningAria2::start_rpc(&common_args());
    let torrent_base64 = base64::engine::general_purpose::STANDARD.encode(&torrent);
    let gid = rpc(
        &first,
        1,
        "aria2.addTorrent",
        json!([torrent_base64, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    let third_file_path = format!("/webseed/{TORRENT_NAME}/part-2.bin");
    web_seed
        .wait_for_request(
            &third_file_path,
            "bytes=16384-49151",
            Duration::from_secs(15),
        )
        .await;
    let first_piece_deadline = Instant::now() + Duration::from_secs(15);
    let first_piece_status = loop {
        let status = rpc(
            &first,
            21,
            "aria2.tellStatus",
            json!([&gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == PIECE_LENGTH.to_string() {
            break status;
        }
        assert!(
            Instant::now() < first_piece_deadline,
            "first WebSeed piece did not complete: {status}; requests={:?}",
            web_seed.requests().await
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(first_piece_status["status"], "active");
    assert_eq!(
        first_piece_status["totalLength"],
        (PIECE_LENGTH * 2).to_string()
    );

    for (index, range) in ["bytes=0-8191", "bytes=0-8191", "bytes=0-16383"]
        .into_iter()
        .enumerate()
    {
        let path = format!("/webseed/{TORRENT_NAME}/part-{index}.bin");
        web_seed
            .wait_for_request(&path, range, Duration::from_secs(2))
            .await;
    }
    let piece_zero_request_count = web_seed
        .requests()
        .await
        .iter()
        .filter(|request| request.range != "bytes=16384-49151")
        .count();

    assert_eq!(rpc(&first, 2, "aria2.pause", json!([gid])), gid);
    let paused = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rpc(&first, 3, "aria2.tellStatus", json!([gid, ["status"]]));
        if status["status"] == "paused" {
            break;
        }
        assert!(Instant::now() < paused, "task did not pause: {status}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rpc(&first, 4, "aria2.saveSession", json!([])), "OK");
    let _ = rpc(&first, 5, "aria2.forceShutdown", json!([]));
    let exit = first.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "first aria2c process exits cleanly: {exit}");

    let torrent_root = output_dir.path().join(TORRENT_NAME);
    let control_path =
        aria2_core::filesystem::control_file::ControlFile::control_path_for(&torrent_root);
    let sidecar = aria2_core::filesystem::control_file::ControlFile::load(&control_path)
        .await
        .expect("load WebSeed checkpoint")
        .expect("paused multi-file WebSeed task has a control file");
    assert_eq!(sidecar.completed_pieces(), 1);
    for (index, expected) in [
        &payload[..FILE_LENGTHS[0]],
        &payload[FILE_LENGTHS[0]..FILE_LENGTHS[0] + FILE_LENGTHS[1]],
        &payload[FILE_LENGTHS[0] + FILE_LENGTHS[1]..PIECE_LENGTH],
    ]
    .into_iter()
    .enumerate()
    {
        let actual = std::fs::read(torrent_root.join(format!("part-{index}.bin")))
            .expect("touched payload file is present after pause");
        assert_eq!(
            &actual[..expected.len()],
            expected,
            "file {index} checkpoint bytes"
        );
    }

    web_seed.release_tail();
    let mut second_args = common_args().to_vec();
    second_args.push(format!("--input-file={}", session_path.display()));
    let mut second = RunningAria2::start_rpc(&second_args);
    let resumed_status = wait_for_field(
        &second,
        &gid,
        "completedLength",
        &PIECE_LENGTH.to_string(),
        Duration::from_secs(10),
    )
    .await;
    assert_eq!(resumed_status["status"], "active");

    let completed = wait_for_field(
        &second,
        &gid,
        "completedLength",
        &(PIECE_LENGTH * 2).to_string(),
        Duration::from_secs(15),
    )
    .await;
    assert_eq!(completed["totalLength"], (PIECE_LENGTH * 2).to_string());
    let _ = rpc(&second, 6, "aria2.forceShutdown", json!([]));
    let exit = second.wait_for_exit(Duration::from_secs(10));
    assert!(
        exit.success(),
        "completed aria2c process exits cleanly: {exit}"
    );

    let requests = web_seed.requests().await;
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.range == "bytes=0-8191" || request.range == "bytes=0-16383"
            })
            .count(),
        piece_zero_request_count,
        "restart must not refetch any file range belonging to completed piece 0"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.path == third_file_path && request.range == "bytes=16384-49151"),
        "the resumed task must fetch the held second piece"
    );
    assert!(
        aria2_core::filesystem::control_file::ControlFile::load(&control_path)
            .await
            .expect("check completed checkpoint cleanup")
            .is_none(),
        "completed multi-file torrent must remove its checkpoint"
    );

    let mut reconstructed = Vec::with_capacity(payload.len());
    for index in 0..FILE_LENGTHS.len() {
        reconstructed.extend_from_slice(
            &std::fs::read(torrent_root.join(format!("part-{index}.bin")))
                .expect("completed WebSeed payload file"),
        );
    }
    assert_eq!(reconstructed, *payload);
}
