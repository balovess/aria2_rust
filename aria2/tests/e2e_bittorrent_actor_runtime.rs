#![cfg(feature = "bittorrent")]

//! Process-level BitTorrent regression coverage through the CLI/RPC boundary.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};

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
        "RPC HTTP response: {:?}",
        response.headers
    );
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");
    assert!(response.get("error").is_none(), "RPC error: {response}");
    response["result"].clone()
}

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve an ephemeral BitTorrent listen port")
        .local_addr()
        .expect("bound socket has a local address")
        .port()
}

fn test_torrent(tracker_url: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(3));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"actor-runtime.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16 * 1024));
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

fn request_window_torrent(tracker_url: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(524_288));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"request-window.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(524_288));
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(vec![
            0x3a, 0xc8, 0x8e, 0x6e, 0x5d, 0xdb, 0xf1, 0x43, 0x27, 0x18, 0x87, 0x94, 0xb4, 0x7f,
            0x66, 0x2f, 0x5b, 0x45, 0xdd, 0x40,
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

struct ControlledSeeder {
    addr: SocketAddr,
    request_count: Arc<AtomicUsize>,
    choked_request_count: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl ControlledSeeder {
    async fn start(info_hash: [u8; 20]) -> Self {
        Self::start_with_unchoke_delay(info_hash, Duration::ZERO).await
    }

    async fn start_with_unchoke_delay(info_hash: [u8; 20], unchoke_delay: Duration) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind controlled seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let request_count = Arc::new(AtomicUsize::new(0));
        let choked_request_count = Arc::new(AtomicUsize::new(0));
        let requests = Arc::clone(&request_count);
        let choked_requests = Arc::clone(&choked_request_count);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let requests = Arc::clone(&requests);
                let choked_requests = Arc::clone(&choked_requests);
                tokio::spawn(async move {
                    serve_peer(
                        &mut stream,
                        info_hash,
                        requests,
                        choked_requests,
                        unchoke_delay,
                    )
                    .await;
                });
            }
        });
        Self {
            addr,
            request_count,
            choked_request_count,
            task,
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }

    fn choked_request_count(&self) -> usize {
        self.choked_request_count.load(Ordering::SeqCst)
    }
}

impl Drop for ControlledSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct WindowedSeeder {
    addr: SocketAddr,
    peak_outstanding: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl WindowedSeeder {
    async fn start(info_hash: [u8; 20], piece_data: Arc<Vec<u8>>) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind delayed seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let peak_outstanding = Arc::new(AtomicUsize::new(0));
        let peak = Arc::clone(&peak_outstanding);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        connections.spawn(serve_windowed_peer(
                            stream,
                            info_hash,
                            Arc::clone(&piece_data),
                            Arc::clone(&peak),
                        ));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            peak_outstanding,
            task,
        }
    }

    fn peak_outstanding(&self) -> usize {
        self.peak_outstanding.load(Ordering::SeqCst)
    }
}

impl Drop for WindowedSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_windowed_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    piece_data: Arc<Vec<u8>>,
    peak_outstanding: Arc<AtomicUsize>,
) {
    let mut handshake = [0u8; 68];
    if tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut handshake))
        .await
        .is_err()
    {
        return;
    }
    if &handshake[28..48] != info_hash {
        return;
    }

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[27] |= 0x04;
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(b"WindowSeeder-0000000");
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
    {
        return;
    }

    let (mut reader, writer) = stream.into_split();
    let writer = Arc::new(Mutex::new(writer));
    let outstanding = Arc::new(AtomicUsize::new(0));
    let mut responses = JoinSet::new();
    loop {
        tokio::select! {
            message = read_bt_message(&mut reader) => {
                let Ok(message) = message else { break };
                if message.len() != 13 || message.first() != Some(&6) {
                    continue;
                }
                let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
                let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                let Some(end) = begin.checked_add(length) else { continue };
                if index != 0 || end > piece_data.len() {
                    continue;
                }
                let active = outstanding.fetch_add(1, Ordering::SeqCst) + 1;
                peak_outstanding.fetch_max(active, Ordering::SeqCst);
                let block = piece_data[begin..end].to_vec();
                let writer = Arc::clone(&writer);
                let outstanding = Arc::clone(&outstanding);
                responses.spawn(async move {
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    let mut response = Vec::with_capacity(13 + block.len());
                    response.extend_from_slice(&((9 + block.len()) as u32).to_be_bytes());
                    response.push(7);
                    response.extend_from_slice(&index.to_be_bytes());
                    response.extend_from_slice(&(begin as u32).to_be_bytes());
                    response.extend_from_slice(&block);
                    let result = writer.lock().await.write_all(&response).await;
                    outstanding.fetch_sub(1, Ordering::SeqCst);
                    result
                });
            }
            Some(_) = responses.join_next(), if !responses.is_empty() => {}
        }
    }
    responses.abort_all();
}

async fn read_bt_message(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let length = stream.read_u32().await?;
    if length == 0 {
        return Ok(Vec::new());
    }
    if length > 64 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "BT message exceeds test limit",
        ));
    }
    let mut message = vec![0; length as usize];
    stream.read_exact(&mut message).await?;
    Ok(message)
}

async fn serve_peer(
    stream: &mut TcpStream,
    info_hash: [u8; 20],
    request_count: Arc<AtomicUsize>,
    choked_request_count: Arc<AtomicUsize>,
    unchoke_delay: Duration,
) {
    let mut handshake = [0u8; 68];
    if tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut handshake))
        .await
        .is_err()
        || &handshake[28..48] != info_hash
    {
        return;
    }

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(b"ActorSeeder-00000001");
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
    {
        return;
    }

    if !unchoke_delay.is_zero() && stream.write_all(&[0, 0, 0, 1, 0]).await.is_err() {
        return;
    }
    let unchoke_at = Instant::now() + unchoke_delay;
    let mut early_request = None;
    let mut interested = false;
    loop {
        if interested && Instant::now() >= unchoke_at {
            if stream.write_all(&[0, 0, 0, 1, 1]).await.is_err() {
                return;
            }
            break;
        }
        let message = if Instant::now() < unchoke_at {
            let wait = unchoke_at.saturating_duration_since(Instant::now());
            match tokio::time::timeout(wait, read_bt_message(stream)).await {
                Ok(Ok(message)) => message,
                Ok(Err(_)) => return,
                Err(_) => continue,
            }
        } else {
            match tokio::time::timeout(Duration::from_secs(5), read_bt_message(stream)).await {
                Ok(Ok(message)) => message,
                Ok(Err(_)) | Err(_) => return,
            }
        };
        if message.first() == Some(&2) {
            interested = true;
        } else if message.first() == Some(&6) {
            choked_request_count.fetch_add(1, Ordering::SeqCst);
            early_request = Some(message);
        }
    }

    loop {
        let message = if let Some(message) = early_request.take() {
            message
        } else {
            match read_bt_message(stream).await {
                Ok(message) => message,
                Err(_) => return,
            }
        };
        if message.is_empty() {
            continue;
        }
        if message.len() != 13 || message.first() != Some(&6) {
            continue;
        }
        let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap());
        let requested = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
        if index != 0 || begin != 0 || requested != 3 {
            return;
        }
        let is_first_request = request_count.fetch_add(1, Ordering::SeqCst) == 0;
        if is_first_request && unchoke_delay.is_zero() {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let mut piece = Vec::with_capacity(16);
        piece.extend_from_slice(&[0, 0, 0, 12, 7]);
        piece.extend_from_slice(&index.to_be_bytes());
        piece.extend_from_slice(&begin.to_be_bytes());
        piece.extend_from_slice(b"abc");
        let _ = stream.write_all(&piece).await;
    }
}

#[tokio::test]
async fn cli_waits_for_unchoke_before_requesting_a_non_fast_piece() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer = ControlledSeeder::start_with_unchoke_delay(
        meta.info_hash.bytes,
        Duration::from_millis(300),
    )
    .await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = test_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=1".to_owned(),
        "--seed-ratio=0".to_owned(),
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

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if peer.request_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the peer was never asked for the piece after unchoking");
    assert_eq!(
        peer.choked_request_count(),
        0,
        "the client must not request a non-AllowedFast piece while the peer is choking it"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "3" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "download did not complete: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        std::fs::read(output_dir.path().join("actor-runtime.bin"))
            .expect("completed payload is readable"),
        b"abc"
    );
}

#[tokio::test]
async fn cli_expands_peer_request_window_after_successful_block_responses() {
    const PIECE_LENGTH: usize = 524_288;
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let piece = Arc::new(vec![0x4a; PIECE_LENGTH]);
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = request_window_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("request-window torrent metadata parses");
    let peer = WindowedSeeder::start(meta.info_hash.bytes, Arc::clone(&piece)).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = request_window_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=1".to_owned(),
        "--seed-ratio=0".to_owned(),
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

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == PIECE_LENGTH.to_string() {
            break;
        }
        if Instant::now() >= deadline {
            let peers = rpc(&client, 3, "aria2.getPeers", json!([gid]));
            let trackers = rpc(&client, 4, "aria2.getTrackers", json!([gid]));
            let queries = tracker.captured_queries().await;
            panic!(
                "delayed peer did not complete the piece: {status}; peak outstanding {}; seeder task finished {}; peers: {peers}; trackers: {trackers}; tracker queries: {queries:?}",
                peer.peak_outstanding(),
                peer.task.is_finished()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        peer.peak_outstanding() > 6,
        "the request window should grow beyond aria2's initial six blocks; observed peak {}",
        peer.peak_outstanding()
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("request-window.bin"))
            .expect("completed payload is readable"),
        *piece,
        "complete piece bytes are persisted after verification"
    );
}

async fn read_peer_message(stream: &mut TcpStream) -> Vec<u8> {
    loop {
        let length = stream.read_u32().await.expect("read peer message length");
        if length == 0 {
            continue;
        }
        assert!(length <= 64 * 1024, "peer message exceeds test limit");
        let mut message = vec![0u8; length as usize];
        stream
            .read_exact(&mut message)
            .await
            .expect("read complete peer message");
        return message;
    }
}

async fn read_peer_message_or_disconnect(stream: &mut TcpStream) -> Option<Vec<u8>> {
    loop {
        let length = stream.read_u32().await.ok()?;
        if length == 0 {
            continue;
        }
        if length > 64 * 1024 {
            return None;
        }
        let mut message = vec![0u8; length as usize];
        stream.read_exact(&mut message).await.ok()?;
        return Some(message);
    }
}

#[tokio::test]
async fn cli_download_keeps_live_peer_visible_during_actor_transfer() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer = ControlledSeeder::start(meta.info_hash.bytes).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = test_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=1".to_owned(),
        "--seed-ratio=0".to_owned(),
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

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if peer.request_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("CLI download never issued a block request to the connected peer");

    let peers = rpc(&client, 2, "aria2.getPeers", json!([gid]));
    assert!(
        peers.as_array().is_some_and(|peers| !peers.is_empty()),
        "the connected peer must be visible in the public RPC snapshot while its block response is in flight"
    );
    let peer_details = rpc(&client, 6, "aria2.getPeerDetails", json!([gid]));
    assert!(
        peer_details.as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["seeder"].as_bool() == Some(true))
        }),
        "a full peer bitfield must be reflected as seeder=true in RPC: {peer_details}"
    );
    assert!(
        peer_details.as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["flags"]["amInterested"].as_bool() == Some(true))
        }),
        "a peer with an outstanding block request must be reported as interesting in RPC: {peer_details}"
    );

    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = rpc(
            &client,
            3,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "3" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "download did not complete: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let downloaded = std::fs::read(output_dir.path().join("actor-runtime.bin"))
        .expect("completed payload is readable");
    assert_eq!(
        downloaded, b"abc",
        "completed payload matches verified piece"
    );

    tracker.wait_for_event("completed").await;
    let trackers = rpc(&client, 4, "aria2.getTrackers", json!([gid]));
    assert!(
        trackers
            .as_array()
            .is_some_and(|entries| entries.iter().any(|entry| {
                entry["uri"] == tracker.announce_url() && entry["status"] == "succeeded"
            })),
        "successful live tracker state is exposed through RPC: {trackers}"
    );

    let mut leecher = TcpStream::connect((
        "127.0.0.1",
        args[1]["--listen-port=".len()..].parse::<u16>().unwrap(),
    ))
    .await
    .expect("connect a controlled leecher to the active seeding listener");
    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&meta.info_hash.bytes);
    handshake[48..68].copy_from_slice(b"ControlledLeecher-01");
    leecher
        .write_all(&handshake)
        .await
        .expect("send controlled leecher handshake");
    let mut response = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(5), leecher.read_exact(&mut response))
        .await
        .expect("seeding listener handshake timed out")
        .expect("read seeding listener handshake");
    assert_eq!(
        &response[28..48],
        &meta.info_hash.bytes,
        "seeding listener accepted this torrent's info hash"
    );

    leecher
        .write_all(&[0, 0, 0, 1, 2])
        .await
        .expect("send Interested to the seeding actor");
    let got_unchoke = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if read_peer_message(&mut leecher).await.first() == Some(&1) {
                break;
            }
        }
    })
    .await
    .is_ok();
    assert!(
        got_unchoke,
        "seeding actor did not unchoke the interested leecher"
    );

    let mut request = Vec::with_capacity(17);
    request.extend_from_slice(&13u32.to_be_bytes());
    request.push(6);
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&3u32.to_be_bytes());
    leecher
        .write_all(&request)
        .await
        .expect("request the downloaded piece from the seeding actor");
    let uploaded_piece = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = read_peer_message(&mut leecher).await;
            if message.first() == Some(&7) {
                break message;
            }
        }
    })
    .await
    .expect("seeding actor did not upload the requested piece");
    assert_eq!(uploaded_piece.len(), 12);
    assert_eq!(&uploaded_piece[1..5], &0u32.to_be_bytes());
    assert_eq!(&uploaded_piece[5..9], &0u32.to_be_bytes());
    assert_eq!(&uploaded_piece[9..], b"abc");

    let details = rpc(&client, 5, "aria2.getPeerDetails", json!([gid]));
    assert!(
        details
            .as_array()
            .is_some_and(|peers| peers.iter().any(|peer| {
                peer["source"] == "incoming"
                    && peer["ip"] == "127.0.0.1"
                    && peer["uploadedBytes"]
                        .as_str()
                        .is_some_and(|bytes| bytes.parse::<u64>().unwrap_or(0) >= 3)
            })),
        "incoming upload counters are visible through peer RPC: {details}"
    );
}

fn two_piece_upload_torrent(tracker_url: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(32));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"active-upload.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16));
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(
            [
                0x19, 0xb1, 0x92, 0x8d, 0x58, 0xa2, 0x03, 0x0d, 0x08, 0x02, 0x3f, 0x3d, 0x70, 0x54,
                0x51, 0x6d, 0xbc, 0x18, 0x6f, 0x20, 0xeb, 0xa6, 0x29, 0x20, 0x22, 0xb9, 0xd8, 0xaf,
                0xd8, 0x9b, 0x10, 0x1c, 0x23, 0x55, 0xe1, 0x79, 0x07, 0x93, 0xfb, 0x3b,
            ]
            .to_vec(),
        ),
    );
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

struct PartialSeeder {
    addr: SocketAddr,
    upload_request: Arc<tokio::sync::Notify>,
    release_first_piece: Arc<tokio::sync::Notify>,
    release_tail: Arc<tokio::sync::Notify>,
    uploaded_bytes: Arc<AtomicUsize>,
    peer_unchoked: Arc<std::sync::atomic::AtomicBool>,
    peer_interested: Arc<tokio::sync::Notify>,
    peer_not_interested: Arc<tokio::sync::Notify>,
    task: JoinHandle<()>,
}

impl PartialSeeder {
    async fn start(info_hash: [u8; 20], payload: Arc<Vec<u8>>) -> Self {
        Self::start_with_bitfield(info_hash, payload, 0x80).await
    }

    async fn start_with_bitfield(
        info_hash: [u8; 20],
        payload: Arc<Vec<u8>>,
        initial_bitfield: u8,
    ) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind partial seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let upload_request = Arc::new(tokio::sync::Notify::new());
        let release_first_piece = Arc::new(tokio::sync::Notify::new());
        let release_tail = Arc::new(tokio::sync::Notify::new());
        let upload_request_task = upload_request.clone();
        let release_first_piece_task = release_first_piece.clone();
        let release_tail_task = release_tail.clone();
        let uploaded_bytes = Arc::new(AtomicUsize::new(0));
        let uploaded_bytes_task = Arc::clone(&uploaded_bytes);
        let peer_unchoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peer_unchoked_task = Arc::clone(&peer_unchoked);
        let peer_interested = Arc::new(tokio::sync::Notify::new());
        let peer_interested_task = Arc::clone(&peer_interested);
        let peer_not_interested = Arc::new(tokio::sync::Notify::new());
        let peer_not_interested_task = Arc::clone(&peer_not_interested);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        connections.spawn(serve_partial_peer(
                            stream,
                            info_hash,
                            Arc::clone(&payload),
                            initial_bitfield,
                            upload_request_task.clone(),
                            release_first_piece_task.clone(),
                            release_tail_task.clone(),
                            Arc::clone(&uploaded_bytes_task),
                            Arc::clone(&peer_unchoked_task),
                            Arc::clone(&peer_interested_task),
                            Arc::clone(&peer_not_interested_task),
                        ));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            upload_request,
            release_first_piece,
            release_tail,
            uploaded_bytes,
            peer_unchoked,
            peer_interested,
            peer_not_interested,
            task,
        }
    }

    fn release_first_piece(&self) {
        self.release_first_piece.notify_one();
    }

    fn request_upload(&self) {
        self.upload_request.notify_one();
    }

    fn release_tail_piece(&self) {
        self.release_tail.notify_one();
    }
}

impl Drop for PartialSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_partial_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    payload: Arc<Vec<u8>>,
    initial_bitfield: u8,
    upload_request: Arc<tokio::sync::Notify>,
    release_first_piece: Arc<tokio::sync::Notify>,
    release_tail: Arc<tokio::sync::Notify>,
    uploaded_bytes: Arc<AtomicUsize>,
    peer_unchoked: Arc<std::sync::atomic::AtomicBool>,
    peer_interested: Arc<tokio::sync::Notify>,
    peer_not_interested: Arc<tokio::sync::Notify>,
) {
    let mut request_handshake = [0u8; 68];
    if tokio::time::timeout(
        Duration::from_secs(3),
        stream.read_exact(&mut request_handshake),
    )
    .await
    .is_err()
        || request_handshake[0] != 19
        || &request_handshake[28..48] != info_hash
    {
        return;
    }
    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].fill(0x53);
    let bitfield = [0, 0, 0, 2, 5, initial_bitfield];
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&bitfield).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 2]).await.is_err()
    {
        return;
    }

    let mut upload_request_sent = false;
    let mut first_piece_released = initial_bitfield & 0x80 != 0;
    let mut tail_released = false;
    loop {
        let message = tokio::select! {
            message = read_peer_message_or_disconnect(&mut stream) => {
                let Some(message) = message else { return };
                Some(message)
            }
            _ = upload_request.notified(), if !upload_request_sent => {
                let mut request = Vec::with_capacity(17);
                request.extend_from_slice(&13u32.to_be_bytes());
                request.push(6);
                request.extend_from_slice(&0u32.to_be_bytes());
                request.extend_from_slice(&0u32.to_be_bytes());
                request.extend_from_slice(&16u32.to_be_bytes());
                if stream.write_all(&request).await.is_err() {
                    return;
                }
                upload_request_sent = true;
                None
            }
            _ = release_first_piece.notified(), if !first_piece_released => {
                let mut have = Vec::with_capacity(9);
                have.extend_from_slice(&5u32.to_be_bytes());
                have.push(4);
                have.extend_from_slice(&0u32.to_be_bytes());
                if stream.write_all(&have).await.is_err() {
                    return;
                }
                first_piece_released = true;
                None
            }
            _ = release_tail.notified(), if !tail_released => {
                if stream.write_all(&[0, 0, 0, 1, 14]).await.is_err() {
                    return;
                }
                tail_released = true;
                None
            }
        };
        let Some(message) = message else {
            continue;
        };
        match message.first().copied() {
            Some(1) => peer_unchoked.store(true, Ordering::SeqCst),
            Some(2) => peer_interested.notify_one(),
            Some(3) => peer_not_interested.notify_one(),
            Some(6) if message.len() == 13 => {
                let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
                let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                let start = index as usize * 16 + begin;
                let Some(end) = start.checked_add(length) else {
                    continue;
                };
                if end > payload.len() || index > 1 {
                    continue;
                }
                let mut piece = Vec::with_capacity(13 + length);
                piece.extend_from_slice(&((9 + length) as u32).to_be_bytes());
                piece.push(7);
                piece.extend_from_slice(&index.to_be_bytes());
                piece.extend_from_slice(&(begin as u32).to_be_bytes());
                piece.extend_from_slice(&payload[start..end]);
                if stream.write_all(&piece).await.is_err() {
                    return;
                }
            }
            Some(7) if message.len() >= 9 => {
                let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                if index == 0 {
                    uploaded_bytes.fetch_add(message.len() - 9, Ordering::SeqCst);
                }
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn cli_uploads_a_verified_piece_before_torrent_completion() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = two_piece_upload_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("two-piece torrent metadata parses");
    let payload = Arc::new([vec![0x41; 16], vec![0x42; 16]].concat());
    let peer =
        PartialSeeder::start_with_bitfield(meta.info_hash.bytes, Arc::clone(&payload), 0).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = two_piece_upload_torrent(&tracker.announce_url());
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
            if peer.peer_unchoked.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("remote seeder was not unchoked");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), peer.peer_interested.notified())
            .await
            .is_err(),
        "client must not send Interested before the peer advertises a wanted piece"
    );
    peer.release_first_piece();
    tokio::time::timeout(Duration::from_secs(5), peer.peer_interested.notified())
        .await
        .expect("client did not become interested after the peer advertised a wanted piece");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["completedLength"] == "16" {
                assert_eq!(
                    status["status"], "active",
                    "tail piece must still be pending"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first verified piece did not complete while the tail remained unavailable");
    tokio::time::timeout(Duration::from_secs(5), peer.peer_not_interested.notified())
        .await
        .expect("client did not withdraw interest after exhausting this peer's wanted pieces");
    let uninterested_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let peer_details = rpc(&client, 4, "aria2.getPeerDetails", json!([gid]));
        if peer_details[0]["flags"]["amInterested"] == false {
            break;
        }
        assert!(
            Instant::now() < uninterested_deadline,
            "RPC did not publish the NotInterested transition before upload: {peer_details}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    peer.request_upload();
    tokio::time::timeout(Duration::from_secs(5), async {
        while peer.uploaded_bytes.load(Ordering::SeqCst) < 16 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("active download actor did not upload its already-verified piece");

    let active_status = rpc(
        &client,
        3,
        "aria2.tellStatus",
        json!([
            gid,
            [
                "status",
                "completedLength",
                "totalLength",
                "uploadLength",
                "uploadSpeed"
            ]
        ]),
    );
    assert_eq!(active_status["status"], "active");
    assert_eq!(active_status["completedLength"], "16");
    assert_eq!(active_status["totalLength"], "32");
    assert_eq!(
        active_status["uploadLength"], "16",
        "RPC upload accounting should update before the download piece pipeline finishes"
    );
    assert!(
        active_status["uploadSpeed"]
            .as_str()
            .and_then(|speed| speed.parse::<u64>().ok())
            .is_some_and(|speed| speed > 0),
        "RPC should expose a non-zero instantaneous uploadSpeed while the verified piece is being uploaded: {active_status}"
    );
    let peer_snapshot_deadline = Instant::now() + Duration::from_secs(1);
    let peer_details = loop {
        let peer_details = rpc(&client, 4, "aria2.getPeerDetails", json!([gid]));
        if peer_details[0]["uploadedBytes"] == "16" {
            break peer_details;
        }
        assert!(
            Instant::now() < peer_snapshot_deadline,
            "per-peer RPC upload bytes did not reflect the 16 bytes sent during the active download: {peer_details}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        peer_details[0]["flags"]["amInterested"], false,
        "the upload event must not revert the peer's NotInterested state: {peer_details}"
    );

    peer.release_tail_piece();
    tokio::time::timeout(Duration::from_secs(5), peer.peer_interested.notified())
        .await
        .expect("client did not become interested after the peer advertised the remaining piece");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &client,
            4,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "32" {
            assert_eq!(
                status["status"], "active",
                "task remains active while the configured seeding phase runs"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "tail piece did not finish: {status}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::timeout(Duration::from_secs(5), peer.peer_not_interested.notified())
        .await
        .expect("completed torrent did not withdraw Interested from its peer");
    let seeding_snapshot_deadline = Instant::now() + Duration::from_secs(1);
    let seeding_peer_details = loop {
        let peer_details = rpc(&client, 5, "aria2.getPeerDetails", json!([gid]));
        if peer_details[0]["flags"]["amInterested"] == false {
            break peer_details;
        }
        assert!(
            Instant::now() < seeding_snapshot_deadline,
            "RPC did not publish the post-download NotInterested state within one second: {peer_details}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(
        seeding_peer_details[0]["flags"]["amInterested"], false,
        "RPC snapshot reflects the post-download NotInterested state: {seeding_peer_details}"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("active-upload.bin")).unwrap(),
        *payload
    );
}

#[tokio::test]
async fn cli_admits_and_uploads_to_an_incoming_peer_during_piece_download() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = two_piece_upload_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("two-piece torrent metadata parses");
    let payload = Arc::new([vec![0x41; 16], vec![0x42; 16]].concat());
    let peer = PartialSeeder::start(meta.info_hash.bytes, Arc::clone(&payload)).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = two_piece_upload_torrent(&tracker.announce_url());
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=0".to_owned(),
        "--seed-ratio=0".to_owned(),
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
            if peer.peer_unchoked.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("remote seeder was not unchoked");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["completedLength"] == "16" {
                assert_eq!(
                    status["status"], "active",
                    "tail piece must still be pending"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("first verified piece did not complete while the tail remained unavailable");

    let mut leecher = TcpStream::connect(("127.0.0.1", listen_port))
        .await
        .expect("connect incoming leecher to the active torrent listener");
    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&meta.info_hash.bytes);
    handshake[48..68].fill(0x69);
    leecher
        .write_all(&handshake)
        .await
        .expect("send incoming leecher handshake");
    let mut response = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(5), leecher.read_exact(&mut response))
        .await
        .expect("incoming handshake response timed out")
        .expect("read incoming handshake response");
    assert_eq!(&response[28..48], &meta.info_hash.bytes);
    leecher
        .write_all(&[0, 0, 0, 1, 2])
        .await
        .expect("send Interested before requesting the completed piece");

    let mut saw_bitfield = false;
    let mut saw_unchoke = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !saw_bitfield || !saw_unchoke {
            match read_peer_message(&mut leecher).await.first().copied() {
                Some(1) => saw_unchoke = true,
                Some(5) => saw_bitfield = true,
                _ => {}
            }
        }
    })
    .await
    .expect("incoming peer was not admitted and served while a piece download was in flight");

    let mut request = Vec::with_capacity(17);
    request.extend_from_slice(&13u32.to_be_bytes());
    request.push(6);
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&16u32.to_be_bytes());
    leecher
        .write_all(&request)
        .await
        .expect("request already-verified piece from the newly admitted peer");
    let piece = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = read_peer_message(&mut leecher).await;
            if message.first() == Some(&7) {
                break message;
            }
        }
    })
    .await
    .expect("new incoming actor did not upload the verified piece");
    assert_eq!(&piece[1..5], &0u32.to_be_bytes());
    assert_eq!(&piece[5..9], &0u32.to_be_bytes());
    assert_eq!(&piece[9..], &payload[..16]);

    let active_status = rpc(
        &client,
        3,
        "aria2.tellStatus",
        json!([
            gid,
            ["status", "completedLength", "totalLength", "uploadLength"]
        ]),
    );
    assert_eq!(active_status["status"], "active");
    assert_eq!(active_status["completedLength"], "16");
    assert_eq!(active_status["uploadLength"], "16");

    peer.release_tail_piece();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &client,
            4,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["status"] == "complete" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "tail piece did not finish: {status}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read(output_dir.path().join("active-upload.bin")).unwrap(),
        *payload
    );
}
