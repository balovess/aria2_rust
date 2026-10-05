#![cfg(feature = "bittorrent")]

//! CLI/RPC restart coverage for multi-file BitTorrent checkpoint durability.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_core::checksum::message_digest::{HashType, MessageDigest};
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::{Mutex, watch};
use tokio::task::{JoinHandle, JoinSet};

const PIECE_LENGTH: usize = 32 * 1024;
const BLOCK_LENGTH: usize = 16 * 1024;
const FILE_LENGTHS: [usize; 3] = [8 * 1024, 24 * 1024, 32 * 1024];

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
    assert_eq!(
        response.status, 200,
        "RPC HTTP response: {}",
        response.status
    );
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

fn multi_file_torrent(tracker_url: &str, payload: &[u8]) -> Vec<u8> {
    assert_eq!(payload.len(), PIECE_LENGTH * 2);
    let mut pieces = Vec::with_capacity(40);
    for piece in payload.chunks(PIECE_LENGTH) {
        pieces.extend_from_slice(&MessageDigest::hash_data(HashType::Sha1, piece));
    }

    let file_names = [b"first.bin".as_slice(), b"second.bin", b"third.bin"];
    let files = FILE_LENGTHS
        .iter()
        .zip(file_names)
        .map(|(&length, name)| {
            let mut file = BTreeMap::new();
            file.insert(b"length".to_vec(), BencodeValue::Int(length as i64));
            file.insert(
                b"path".to_vec(),
                BencodeValue::List(vec![BencodeValue::Bytes(name.to_vec())]),
            );
            BencodeValue::Dict(file)
        })
        .collect();

    let mut info = BTreeMap::new();
    info.insert(b"files".to_vec(), BencodeValue::List(files));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"checkpoint-multi".to_vec()),
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
    BencodeValue::Dict(root).encode()
}

struct TestSeeder {
    address: SocketAddr,
    connection_count: Arc<AtomicUsize>,
    held_piece_requests: Arc<AtomicBool>,
    tail_piece_available: watch::Sender<bool>,
    requests: Arc<Mutex<HashMap<(u32, u32), usize>>>,
    task: JoinHandle<()>,
}

impl TestSeeder {
    async fn start(info_hash: [u8; 20], payload: Arc<Vec<u8>>) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test seeder");
        let address = listener.local_addr().expect("seeder address");
        let connection_count = Arc::new(AtomicUsize::new(0));
        let held_piece_requests = Arc::new(AtomicBool::new(true));
        let (tail_piece_available, _) = watch::channel(false);
        let requests = Arc::new(Mutex::new(HashMap::new()));

        let connection_count_task = Arc::clone(&connection_count);
        let held_piece_requests_task = Arc::clone(&held_piece_requests);
        let tail_piece_available_task = tail_piece_available.clone();
        let requests_task = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        connection_count_task.fetch_add(1, Ordering::SeqCst);
                        connections.spawn(serve_peer(
                            stream,
                            info_hash,
                            Arc::clone(&payload),
                            Arc::clone(&held_piece_requests_task),
                            tail_piece_available_task.subscribe(),
                            Arc::clone(&requests_task),
                        ));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });

        Self {
            address,
            connection_count,
            held_piece_requests,
            tail_piece_available,
            requests,
            task,
        }
    }

    fn release_held_requests(&self) {
        self.held_piece_requests.store(false, Ordering::SeqCst);
    }

    fn advertise_tail_piece(&self) {
        self.tail_piece_available.send_replace(true);
    }

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    async fn request_count(&self, piece: u32, offset: u32) -> usize {
        self.requests
            .lock()
            .await
            .get(&(piece, offset))
            .copied()
            .unwrap_or_default()
    }
}

impl Drop for TestSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    payload: Arc<Vec<u8>>,
    held_piece_requests: Arc<AtomicBool>,
    mut tail_piece_available: watch::Receiver<bool>,
    requests: Arc<Mutex<HashMap<(u32, u32), usize>>>,
) {
    let mut request_handshake = [0u8; 68];
    if tokio::time::timeout(
        Duration::from_secs(5),
        stream.read_exact(&mut request_handshake),
    )
    .await
    .is_err()
        || request_handshake[0] != 19
        || request_handshake[28..48] != info_hash
    {
        return;
    }

    let mut peer_id = [b'P'; 20];
    peer_id[18..].copy_from_slice(
        &stream
            .peer_addr()
            .map_or(0, |peer| peer.port())
            .to_be_bytes(),
    );
    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(&peer_id);
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
    {
        return;
    }

    let mut tail_piece_announced = false;
    loop {
        if *tail_piece_available.borrow_and_update() && !tail_piece_announced {
            let mut have = [0u8; 9];
            have[..4].copy_from_slice(&5u32.to_be_bytes());
            have[4] = 4;
            have[5..].copy_from_slice(&1u32.to_be_bytes());
            if stream.write_all(&have).await.is_err() {
                return;
            }
            tail_piece_announced = true;
        }

        tokio::select! {
            changed = tail_piece_available.changed(), if !tail_piece_announced => {
                if changed.is_err() {
                    return;
                }
            }
            message = read_message(&mut stream) => {
                let Some(message) = message else { return };
                if message.len() != 13 || message[0] != 6 {
                    continue;
                }
                let piece = u32::from_be_bytes(message[1..5].try_into().expect("piece index"));
                let offset = u32::from_be_bytes(message[5..9].try_into().expect("block offset"));
                let length = u32::from_be_bytes(message[9..13].try_into().expect("block length"));
                *requests.lock().await.entry((piece, offset)).or_default() += 1;
                if (piece == 0 && offset != 0 && held_piece_requests.load(Ordering::SeqCst))
                    || (piece == 1 && !*tail_piece_available.borrow())
                {
                    continue;
                }

                let start = piece as usize * PIECE_LENGTH + offset as usize;
                let end = start.saturating_add(length as usize);
                let Some(data) = payload.get(start..end) else { continue };
                let mut response = Vec::with_capacity(13 + data.len());
                response.extend_from_slice(&(9u32 + data.len() as u32).to_be_bytes());
                response.push(7);
                response.extend_from_slice(&piece.to_be_bytes());
                response.extend_from_slice(&offset.to_be_bytes());
                response.extend_from_slice(data);
                if stream.write_all(&response).await.is_err() {
                    return;
                }
            }
        }
    }
}

async fn read_message(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.ok()?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 {
        return Some(Vec::new());
    }
    let mut message = vec![0u8; length];
    stream.read_exact(&mut message).await.ok()?;
    Some(message)
}

async fn wait_for_status(
    client: &RunningAria2,
    gid: &str,
    desired_status: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        let status = rpc(client, 20, "aria2.tellStatus", json!([gid, ["status"]]));
        if status["status"] == desired_status {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "wanted {desired_status}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn cli_resumes_multifile_checkpoint_and_syncs_touched_files() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = Arc::new([vec![0x41; PIECE_LENGTH], vec![0x42; PIECE_LENGTH]].concat());
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = multi_file_torrent(&placeholder_tracker.announce_url(), &payload);
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("multi-file torrent metadata parses");
    let peer = TestSeeder::start(meta.info_hash.bytes, Arc::clone(&payload)).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.address.port()).await;
    let torrent = multi_file_torrent(&tracker.announce_url(), &payload);
    let session_path = output_dir.path().join("multi-session.txt");
    let listen_port = reserve_loopback_port();
    let common_args = || {
        [
            format!("--dir={}", output_dir.path().display()),
            format!("--listen-port={listen_port}"),
            "--enable-dht=false".to_owned(),
            "--enable-public-trackers=false".to_owned(),
            "--enable-peer-exchange=false".to_owned(),
            "--bt-enable-web-seed=false".to_owned(),
            "--seed-time=3600".to_owned(),
            format!("--save-session={}", session_path.display()),
        ]
    };

    let mut first = RunningAria2::start_rpc(&common_args());
    let torrent_base64 = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(
        &first,
        1,
        "aria2.addTorrent",
        json!([torrent_base64, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    let first_block_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let peers = rpc(&first, 2, "aria2.getPeerDetails", json!([gid]));
        if peers.as_array().is_some_and(|peers| {
            peers.iter().any(|peer| {
                peer["downloadedBytes"]
                    .as_str()
                    .and_then(|bytes| bytes.parse::<usize>().ok())
                    .is_some_and(|bytes| bytes >= BLOCK_LENGTH)
            })
        }) {
            break;
        }
        assert!(
            Instant::now() < first_block_deadline,
            "first block not received: {peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        peer.request_count(0, 0).await > 0,
        "first block must be requested"
    );
    let first_block_request_count = peer.request_count(0, 0).await;

    assert_eq!(rpc(&first, 3, "aria2.pause", json!([gid])), gid);
    wait_for_status(&first, &gid, "paused", Duration::from_secs(5)).await;
    assert_eq!(rpc(&first, 4, "aria2.saveSession", json!([])), "OK");
    let _ = rpc(&first, 5, "aria2.forceShutdown", json!([]));
    let exit = first.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "first aria2c process exits cleanly: {exit}");

    let torrent_root = output_dir.path().join("checkpoint-multi");
    let control_path =
        aria2_core::filesystem::control_file::ControlFile::control_path_for(&torrent_root);
    let sidecar = aria2_core::filesystem::control_file::ControlFile::load(&control_path)
        .await
        .expect("load paused multi-file checkpoint")
        .expect("paused torrent has a control file");
    assert_eq!(
        sidecar
            .in_flight_pieces()
            .iter()
            .find(|piece| piece.index == 0)
            .map(|piece| piece.bitfield.as_slice()),
        Some([0x80].as_slice()),
        "sidecar must preserve the received block after syncing every touched file"
    );
    let first_file = std::fs::read(torrent_root.join("first.bin"))
        .expect("first touched file is present after pause");
    let second_file = std::fs::read(torrent_root.join("second.bin"))
        .expect("second touched file is present after pause");
    assert_eq!(first_file, payload[..FILE_LENGTHS[0]]);
    assert_eq!(
        &second_file[..BLOCK_LENGTH - FILE_LENGTHS[0]],
        &payload[FILE_LENGTHS[0]..BLOCK_LENGTH]
    );

    peer.release_held_requests();
    let mut second_args = common_args().to_vec();
    second_args.push(format!("--input-file={}", session_path.display()));
    let mut second = RunningAria2::start_rpc(&second_args);
    assert!(
        tracker
            .wait_for_query_count(2, Duration::from_secs(5))
            .await,
        "restored task must announce: {:?}",
        tracker.captured_queries().await
    );
    let reconnect_deadline = Instant::now() + Duration::from_secs(5);
    while peer.connection_count() < 2 {
        assert!(
            Instant::now() < reconnect_deadline,
            "restored task did not reconnect"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let restored_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &second,
            6,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength"]]),
        );
        if status["status"] == "active" {
            break;
        }
        assert!(
            Instant::now() < restored_deadline,
            "task did not restore: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let piece_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = rpc(
            &second,
            7,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == PIECE_LENGTH.to_string() {
            assert_eq!(status["status"], "active", "piece 1 remains unavailable");
            assert_eq!(status["totalLength"], (PIECE_LENGTH * 2).to_string());
            break;
        }
        assert!(
            Instant::now() < piece_deadline,
            "restored piece did not complete: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        peer.request_count(0, 0).await,
        first_block_request_count,
        "restart must not redownload the already checkpointed block"
    );
    assert!(
        peer.request_count(0, BLOCK_LENGTH as u32).await > 0,
        "restart must request the missing block"
    );

    peer.advertise_tail_piece();
    let completion_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = rpc(
            &second,
            8,
            "aria2.tellStatus",
            json!([gid, ["completedLength", "totalLength"]]),
        );
        if status["completedLength"] == status["totalLength"] {
            break;
        }
        if Instant::now() >= completion_deadline {
            let peers = rpc(&second, 9, "aria2.getPeerDetails", json!([gid]));
            panic!(
                "torrent did not finish: {status}; peers={peers}; connections={}; block0={}; block1={}",
                peer.connection_count(),
                peer.request_count(0, 0).await,
                peer.request_count(1, 0).await,
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let _ = rpc(&second, 9, "aria2.forceShutdown", json!([]));
    let exit = second.wait_for_exit(Duration::from_secs(10));
    assert!(
        exit.success(),
        "completed aria2c process exits cleanly: {exit}"
    );

    assert!(
        aria2_core::filesystem::control_file::ControlFile::load(&control_path)
            .await
            .expect("check completed checkpoint cleanup")
            .is_none(),
        "completed multi-file torrent must remove its checkpoint"
    );
    let completed_files = ["first.bin", "second.bin", "third.bin"]
        .map(|name| std::fs::read(torrent_root.join(name)).expect("completed payload file"));
    let mut reconstructed = Vec::with_capacity(payload.len());
    for file in completed_files {
        reconstructed.extend_from_slice(&file);
    }
    assert_eq!(reconstructed, *payload);
}
