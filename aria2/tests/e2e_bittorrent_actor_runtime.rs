#![cfg(feature = "bittorrent")]

//! Process-level BitTorrent regression coverage through the CLI/RPC boundary.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_bt_peer.rs"]
mod mock_bt_peer;
#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_core::checksum::message_digest::{HashType, MessageDigest};
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::message::types::BtMessage;
use base64::Engine as _;
use mock_bt_peer::MockBtPeerServer;
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
use tokio::sync::{Mutex, Notify};
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
        response.status,
        200,
        "RPC HTTP response for {method}: {:?}; body: {}",
        response.headers,
        String::from_utf8_lossy(&response.body)
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

fn test_torrent_with_webseed(tracker_url: &str, webseed_url: &str) -> Vec<u8> {
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
    root.insert(
        b"url-list".to_vec(),
        BencodeValue::Bytes(webseed_url.as_bytes().to_vec()),
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
    request_seen: Arc<Notify>,
    connection_closed: Arc<Notify>,
    fast_message_sent: Arc<Notify>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct ControlledPeerSession {
    info_hash: [u8; 20],
    initial_bitfield: u8,
    request_count: Arc<AtomicUsize>,
    choked_request_count: Arc<AtomicUsize>,
    unchoke_delay: Duration,
    fast_extension_message: Option<BtMessage>,
    negotiate_fast_extension: bool,
    request_seen: Arc<Notify>,
    connection_closed: Arc<Notify>,
    fast_message_sent: Arc<Notify>,
}

impl ControlledSeeder {
    async fn start(info_hash: [u8; 20]) -> Self {
        Self::start_with_unchoke_delay(info_hash, Duration::ZERO).await
    }

    async fn start_with_unchoke_delay(info_hash: [u8; 20], unchoke_delay: Duration) -> Self {
        Self::start_with_behavior(info_hash, unchoke_delay, None, false).await
    }

    async fn start_with_allowed_fast_piece(info_hash: [u8; 20], piece_index: u32) -> Self {
        Self::start_with_behavior(
            info_hash,
            Duration::ZERO,
            Some(BtMessage::AllowedFast { index: piece_index }),
            true,
        )
        .await
    }

    async fn start_sending_unnegotiated_allowed_fast(
        info_hash: [u8; 20],
        piece_index: u32,
    ) -> Self {
        Self::start_sending_unnegotiated_fast_message(
            info_hash,
            BtMessage::AllowedFast { index: piece_index },
        )
        .await
    }

    async fn start_sending_unnegotiated_fast_message(
        info_hash: [u8; 20],
        message: BtMessage,
    ) -> Self {
        Self::start_with_behavior(info_hash, Duration::ZERO, Some(message), false).await
    }

    async fn start_with_behavior(
        info_hash: [u8; 20],
        unchoke_delay: Duration,
        fast_extension_message: Option<BtMessage>,
        negotiate_fast_extension: bool,
    ) -> Self {
        Self::start_with_initial_bitfield_and_behavior(
            info_hash,
            0x80,
            unchoke_delay,
            fast_extension_message,
            negotiate_fast_extension,
        )
        .await
    }

    async fn start_with_initial_bitfield(info_hash: [u8; 20], initial_bitfield: u8) -> Self {
        Self::start_with_initial_bitfield_and_behavior(
            info_hash,
            initial_bitfield,
            Duration::ZERO,
            None,
            false,
        )
        .await
    }

    async fn start_with_initial_bitfield_and_behavior(
        info_hash: [u8; 20],
        initial_bitfield: u8,
        unchoke_delay: Duration,
        fast_extension_message: Option<BtMessage>,
        negotiate_fast_extension: bool,
    ) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind controlled seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let request_count = Arc::new(AtomicUsize::new(0));
        let choked_request_count = Arc::new(AtomicUsize::new(0));
        let request_seen = Arc::new(Notify::new());
        let connection_closed = Arc::new(Notify::new());
        let fast_message_sent = Arc::new(Notify::new());
        let peer_session = ControlledPeerSession {
            info_hash,
            initial_bitfield,
            request_count: Arc::clone(&request_count),
            choked_request_count: Arc::clone(&choked_request_count),
            unchoke_delay,
            fast_extension_message,
            negotiate_fast_extension,
            request_seen: Arc::clone(&request_seen),
            connection_closed: Arc::clone(&connection_closed),
            fast_message_sent: Arc::clone(&fast_message_sent),
        };
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let peer_session = peer_session.clone();
                tokio::spawn(async move {
                    serve_peer(&mut stream, peer_session).await;
                });
            }
        });
        Self {
            addr,
            request_count,
            choked_request_count,
            request_seen,
            connection_closed,
            fast_message_sent,
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

struct PersistentIdlePeer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl PersistentIdlePeer {
    async fn start(info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind persistent idle peer");
        let addr = listener.local_addr().expect("idle peer local address");
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve_persistent_idle_peer(stream, info_hash, peer_id));
            }
        });
        Self { addr, task }
    }
}

impl Drop for PersistentIdlePeer {
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
        Self::start_with_response_delay(info_hash, piece_data, Duration::from_millis(80)).await
    }

    async fn start_with_response_delay(
        info_hash: [u8; 20],
        piece_data: Arc<Vec<u8>>,
        response_delay: Duration,
    ) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind delayed seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let mut peer_id = *b"WindowSeed0000000000";
        peer_id[18..].copy_from_slice(&addr.port().to_be_bytes());
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
                            peer_id,
                            Arc::clone(&piece_data),
                            Arc::clone(&peak),
                            response_delay,
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
    peer_id: [u8; 20],
    piece_data: Arc<Vec<u8>>,
    peak_outstanding: Arc<AtomicUsize>,
    response_delay: Duration,
) {
    let mut handshake = [0u8; 68];
    if tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut handshake))
        .await
        .is_err()
    {
        return;
    }
    if handshake[28..48] != info_hash {
        return;
    }

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[27] |= 0x04;
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(&peer_id);
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
                    tokio::time::sleep(response_delay).await;
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

async fn serve_persistent_idle_peer(mut stream: TcpStream, info_hash: [u8; 20], peer_id: [u8; 20]) {
    let mut handshake = [0u8; 68];
    if !matches!(
        tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut handshake)).await,
        Ok(Ok(_))
    ) || handshake[28..48] != info_hash
    {
        return;
    }

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(&peer_id);
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0]).await.is_err()
    {
        return;
    }

    while read_bt_message(&mut stream).await.is_ok() {}
}

async fn serve_peer(stream: &mut TcpStream, session: ControlledPeerSession) {
    let ControlledPeerSession {
        info_hash,
        initial_bitfield,
        request_count,
        choked_request_count,
        unchoke_delay,
        fast_extension_message,
        negotiate_fast_extension,
        request_seen,
        connection_closed,
        fast_message_sent,
    } = session;
    let mut handshake = [0u8; 68];
    if tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut handshake))
        .await
        .is_err()
        || handshake[28..48] != info_hash
    {
        return;
    }

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    if negotiate_fast_extension {
        response[27] |= 0x04;
    }
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(b"ActorSeeder-00000001");
    if stream.write_all(&response).await.is_err()
        || stream
            .write_all(&[0, 0, 0, 2, 5, initial_bitfield])
            .await
            .is_err()
    {
        return;
    }

    if !unchoke_delay.is_zero() && stream.write_all(&[0, 0, 0, 1, 0]).await.is_err() {
        return;
    }
    if let Some(message) = &fast_extension_message {
        let encoded = aria2_protocol::bittorrent::message::serializer::serialize(message);
        if stream.write_all(&encoded).await.is_err() {
            return;
        }
        fast_message_sent.notify_one();
    }
    let unchoke_at = Instant::now() + unchoke_delay;
    let keep_choked = fast_extension_message.is_some();
    let mut early_request = None;
    let mut interested = false;
    loop {
        if interested && Instant::now() >= unchoke_at && !keep_choked {
            if stream.write_all(&[0, 0, 0, 1, 1]).await.is_err() {
                return;
            }
            break;
        }
        let message = if Instant::now() < unchoke_at {
            let wait = unchoke_at.saturating_duration_since(Instant::now());
            match tokio::time::timeout(wait, read_bt_message(stream)).await {
                Ok(Ok(message)) => message,
                Ok(Err(_)) => {
                    connection_closed.notify_one();
                    return;
                }
                Err(_) => continue,
            }
        } else {
            match tokio::time::timeout(Duration::from_secs(5), read_bt_message(stream)).await {
                Ok(Ok(message)) => message,
                Ok(Err(_)) => {
                    connection_closed.notify_one();
                    return;
                }
                Err(_) => return,
            }
        };
        if message.first() == Some(&2) {
            interested = true;
        } else if message.first() == Some(&6) {
            choked_request_count.fetch_add(1, Ordering::SeqCst);
            request_seen.notify_one();
            early_request = Some(message);
            if keep_choked {
                break;
            }
        }
    }

    loop {
        let message = if let Some(message) = early_request.take() {
            message
        } else {
            match read_bt_message(stream).await {
                Ok(message) => message,
                Err(_) => {
                    connection_closed.notify_one();
                    return;
                }
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
async fn rpc_change_uri_updates_file_uris_while_bt_piece_session_is_active() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent_with_webseed(
        &placeholder_tracker.announce_url(),
        "http://127.0.0.1:1/original.bin",
    );
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer = ControlledSeeder::start(meta.info_hash.bytes).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent =
        test_torrent_with_webseed(&tracker.announce_url(), "http://127.0.0.1:1/original.bin");
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
    .expect("piece session did not become active");

    let original = "http://127.0.0.1:1/original.bin";
    let replacement = "http://127.0.0.1:1/replacement.bin";
    let changed = client.post(
        "/jsonrpc",
        "application/json",
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "aria2.changeUri",
            "params": [gid, 1, [original], [replacement]],
        })
        .to_string()
        .as_bytes(),
    );
    assert_eq!(changed.status, 200);
    let changed: Value = serde_json::from_slice(&changed.body).expect("changeUri JSON response");
    assert!(
        changed.get("error").is_none(),
        "changeUri failed: {changed}"
    );
    assert_eq!(changed["result"], json!(["1", "1"]));

    let files = rpc(&client, 3, "aria2.getFiles", json!([gid]));
    let uris = files[0]["uris"].as_array().expect("file URI list");
    assert!(uris.iter().any(|uri| uri["uri"] == replacement), "{files}");
    assert!(!uris.iter().any(|uri| uri["uri"] == original), "{files}");
}

#[tokio::test]
async fn cli_downloads_allowed_fast_piece_while_peer_keeps_choking() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer = ControlledSeeder::start_with_allowed_fast_piece(meta.info_hash.bytes, 0).await;
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
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["completedLength", "totalLength"]]),
            );
            if status["completedLength"] == "3" && status["totalLength"] == "3" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("AllowedFast piece should complete without an Unchoke");

    assert_eq!(peer.choked_request_count(), 1);
    assert_eq!(peer.request_count(), 1);
    assert_eq!(
        std::fs::read(output_dir.path().join("actor-runtime.bin"))
            .expect("completed payload is readable"),
        b"abc"
    );
}

#[tokio::test]
async fn cli_rejects_allowed_fast_without_fast_extension_negotiation() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer =
        ControlledSeeder::start_sending_unnegotiated_allowed_fast(meta.info_hash.bytes, 0).await;
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

    tokio::time::timeout(Duration::from_secs(5), peer.fast_message_sent.notified())
        .await
        .expect("fixture sent the unsolicited AllowedFast message");
    let disconnected_without_request = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = peer.connection_closed.notified() => true,
            _ = peer.request_seen.notified() => false,
        }
    })
    .await
    .expect("peer should either be rejected or expose an invalid piece request");

    assert!(
        disconnected_without_request,
        "client must reject AllowedFast when Fast Extension was not negotiated"
    );
    assert_eq!(
        peer.request_count(),
        0,
        "rejected peer must receive no request"
    );
    assert_eq!(
        peer.choked_request_count(),
        0,
        "unnegotiated AllowedFast must not bypass choking"
    );

    let _ = rpc(&client, 2, "aria2.forceRemove", json!([gid]));
}

async fn assert_unnegotiated_fast_message_is_rejected(message: BtMessage) {
    let label = format!("{message:?}");
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer =
        ControlledSeeder::start_sending_unnegotiated_fast_message(meta.info_hash.bytes, message)
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

    tokio::time::timeout(Duration::from_secs(5), peer.fast_message_sent.notified())
        .await
        .unwrap_or_else(|_| panic!("fixture did not send {label}"));
    let disconnected_without_request = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            _ = peer.connection_closed.notified() => true,
            _ = peer.request_seen.notified() => false,
        }
    })
    .await
    .unwrap_or_else(|_| panic!("client did not reject {label}"));

    assert!(
        disconnected_without_request,
        "client requested after {label}"
    );
    assert_eq!(peer.request_count(), 0, "client requested after {label}");
    let _ = rpc(&client, 2, "aria2.forceRemove", json!([gid]));
}

#[tokio::test]
async fn cli_rejects_fast_extension_messages_without_negotiation() {
    for message in [
        BtMessage::HaveAll,
        BtMessage::HaveNone,
        BtMessage::Reject {
            index: 0,
            offset: 0,
            length: 3,
        },
    ] {
        assert_unnegotiated_fast_message_is_rejected(message).await;
    }
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
        if status["completedLength"]
            .as_str()
            .and_then(|completed| completed.parse::<usize>().ok())
            == Some(PIECE_LENGTH)
        {
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

#[tokio::test]
async fn cli_fills_aggregate_request_window_across_unchoked_peers() {
    const PEER_COUNT: usize = 4;
    const PIECE_LENGTH: usize = 524_288;
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let piece = Arc::new(vec![0x4a; PIECE_LENGTH]);
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = request_window_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("request-window torrent metadata parses");
    let mut peers = Vec::with_capacity(PEER_COUNT);
    for _ in 0..PEER_COUNT {
        peers.push(
            WindowedSeeder::start_with_response_delay(
                meta.info_hash.bytes,
                Arc::clone(&piece),
                Duration::from_secs(5),
            )
            .await,
        );
    }
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start_with_peers(
        peers.iter().map(|peer| peer.addr.port()).collect(),
        false,
    )
    .await;
    let torrent = request_window_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        format!("--bt-max-peers={PEER_COUNT}"),
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

    let deadline = Instant::now() + Duration::from_secs(3);
    let (peer_details, total_outstanding, peers_with_requests) = loop {
        let details = rpc(&client, 2, "aria2.getPeerDetails", json!([gid]));
        let Some(active_peers) = details.as_array() else {
            panic!("getPeerDetails should return an array: {details}");
        };
        let all_ready = peers.iter().all(|expected| {
            active_peers.iter().any(|actual| {
                actual["port"].as_u64() == Some(u64::from(expected.addr.port()))
                    && actual["flags"]["peerChoking"] == false
            })
        });
        let total_outstanding = active_peers
            .iter()
            .filter_map(|peer| peer["outstandingRequestsToPeer"].as_u64())
            .sum::<u64>();
        let peers_with_requests = active_peers
            .iter()
            .filter(|peer| {
                peer["outstandingRequestsToPeer"]
                    .as_u64()
                    .is_some_and(|count| count > 0)
            })
            .count();
        if all_ready && total_outstanding >= 12 && peers_with_requests >= 3 {
            break (details, total_outstanding, peers_with_requests);
        }
        if Instant::now() >= deadline {
            panic!(
                "the scheduler should promptly distribute requests across unchoked peers; expected ports {:?}, observed requests {total_outstanding} across {peers_with_requests} peers (wire peaks {:?}): {details}",
                peers
                    .iter()
                    .map(|peer| peer.addr.port())
                    .collect::<Vec<_>>(),
                peers
                    .iter()
                    .map(WindowedSeeder::peak_outstanding)
                    .collect::<Vec<_>>()
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    assert!(
        total_outstanding >= 12 && peers_with_requests >= 3,
        "controlled slow seeders should expose aggregate request occupancy: {peer_details}"
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
async fn cli_applies_runtime_peer_limit_to_outbound_tracker_peers() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let initial_peer = PersistentIdlePeer::start(meta.info_hash.bytes, [0x61; 20]).await;
    let additional_peer = PersistentIdlePeer::start(meta.info_hash.bytes, [0x62; 20]).await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start_with_event_peers(
        vec![initial_peer.addr.port()],
        vec![additional_peer.addr.port()],
        1,
    )
    .await;
    let torrent = test_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-max-peers=1".to_owned(),
    ];
    let mut client = RunningAria2::start_rpc(&args);
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

    assert_eq!(
        rpc(&client, 2, "aria2.getOption", json!([gid]))["bt-max-peers"],
        "1"
    );
    tracker.wait_for_event("started").await;
    let initial_peer_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let peers = rpc(&client, 3, "aria2.getPeerDetails", json!([gid]));
        if peers.as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["port"] == initial_peer.addr.port())
        }) {
            assert_eq!(
                peers.as_array().map(Vec::len),
                Some(1),
                "before the option change, only the initial peer should be active: {peers}"
            );
            break;
        }
        assert!(
            Instant::now() < initial_peer_deadline,
            "the started announce peer must occupy the initial one-peer limit: {peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(
        rpc(
            &client,
            4,
            "aria2.changeOption",
            json!([gid, {"bt-max-peers": "2"}]),
        ),
        "OK"
    );
    assert_eq!(
        rpc(&client, 5, "aria2.getOption", json!([gid]))["bt-max-peers"],
        "2"
    );
    assert!(
        tracker
            .wait_for_query_count(2, Duration::from_secs(5))
            .await,
        "the tracker must return the additional peer on its follow-up announce"
    );

    let additional_peer_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let peers = rpc(&client, 6, "aria2.getPeerDetails", json!([gid]));
        if peers.as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["port"] == additional_peer.addr.port())
        }) {
            break;
        }
        assert!(
            Instant::now() < additional_peer_deadline,
            "after bt-max-peers changes from 1 to 2, the outbound tracker peer must be admitted: {peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = rpc(&client, 7, "aria2.forceShutdown", json!([]));
    let exit = client.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "aria2c exits cleanly: {exit}");
}

#[tokio::test]
async fn cli_seeding_reports_tracker_source_for_a_leecher_discovered_after_completion() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    std::fs::write(output_dir.path().join("actor-runtime.bin"), b"abc")
        .expect("preseed the fully verified torrent payload");

    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = test_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let peer = ControlledSeeder::start_with_initial_bitfield(meta.info_hash.bytes, 0).await;
    drop(placeholder_tracker);

    let tracker =
        MockTrackerServer::start_with_event_peers(Vec::new(), vec![peer.addr.port()], 1).await;
    let torrent = test_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--check-integrity=true".to_owned(),
        "--bt-hash-check-seed=true".to_owned(),
        "--seed-time=30".to_owned(),
        "--seed-ratio=1".to_owned(),
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

    let completion_deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "3" {
            break;
        }
        assert!(
            Instant::now() < completion_deadline,
            "preverified torrent did not enter the completed lifecycle: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tracker.wait_for_event("completed").await;
    assert!(
        tracker
            .wait_for_query_count(3, Duration::from_secs(10))
            .await,
        "the seeding tracker actor did not perform its follow-up announce"
    );

    let peer_details_deadline = Instant::now() + Duration::from_secs(10);
    let peer_details = loop {
        let peer_details = rpc(&client, 3, "aria2.getPeerDetails", json!([gid]));
        if peer_details
            .as_array()
            .is_some_and(|peers| peers.iter().any(|peer| peer["source"] == "tracker"))
        {
            break peer_details;
        }
        assert!(
            Instant::now() < peer_details_deadline,
            "a peer returned by the seeding-phase tracker announce must appear with source=tracker: {peer_details}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert!(
        peer_details.as_array().is_some_and(|peers| {
            peers
                .iter()
                .any(|peer| peer["source"] == "tracker" && peer["seeder"].as_bool() == Some(false))
        }),
        "seeding-phase RPC snapshot must preserve tracker source and the leecher bitfield: {peer_details}"
    );
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
        peer_details
            .as_array()
            .is_some_and(|peers| { peers.iter().any(|peer| peer["source"] == "tracker") }),
        "an outbound peer returned by the torrent's tracker must retain source=tracker through connection admission: {peer_details}"
    );
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
    let availability =
        tokio::time::timeout(Duration::from_secs(2), read_peer_message(&mut leecher))
            .await
            .expect("seeding actor did not announce piece availability");
    assert_eq!(
        availability,
        [5, 0x80],
        "a handshaken leecher must receive the complete piece bitfield before other peer messages"
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

    let status = rpc(
        &client,
        7,
        "aria2.tellStatus",
        json!([gid, ["status", "uploadLength"]]),
    );
    assert_eq!(
        status["uploadLength"], "3",
        "task RPC uploadLength must include payload bytes served by the seeding actor: {status}"
    );
}

fn two_piece_upload_torrent(tracker_url: &str) -> Vec<u8> {
    two_piece_torrent_with_payload(tracker_url, &[vec![0x41; 16], vec![0x42; 16]].concat(), 16)
}

fn two_piece_torrent_with_payload(
    tracker_url: &str,
    payload: &[u8],
    piece_length: usize,
) -> Vec<u8> {
    assert_eq!(payload.len(), piece_length * 2);
    let mut pieces = Vec::with_capacity(40);
    for piece in payload.chunks(piece_length) {
        pieces.extend_from_slice(&MessageDigest::hash_data(HashType::Sha1, piece));
    }
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(payload.len() as i64));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"active-upload.bin".to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(piece_length as i64),
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

struct PartialSeeder {
    addr: SocketAddr,
    connection_count: Arc<AtomicUsize>,
    piece_request_counts: Arc<[AtomicUsize; 2]>,
    block_requests: Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
    hold_after_first_block: Arc<std::sync::atomic::AtomicBool>,
    upload_request: Arc<tokio::sync::Notify>,
    release_first_piece: Arc<tokio::sync::Notify>,
    release_tail: Arc<tokio::sync::Notify>,
    uploaded_bytes: Arc<AtomicUsize>,
    peer_unchoked: Arc<std::sync::atomic::AtomicBool>,
    peer_interested: Arc<tokio::sync::Notify>,
    peer_not_interested: Arc<tokio::sync::Notify>,
    task: JoinHandle<()>,
}

struct PartialSeederState {
    peer_id: [u8; 20],
    upload_request: Arc<tokio::sync::Notify>,
    release_first_piece: Arc<tokio::sync::Notify>,
    release_tail: Arc<tokio::sync::Notify>,
    uploaded_bytes: Arc<AtomicUsize>,
    peer_unchoked: Arc<std::sync::atomic::AtomicBool>,
    peer_interested: Arc<tokio::sync::Notify>,
    peer_not_interested: Arc<tokio::sync::Notify>,
    piece_request_counts: Arc<[AtomicUsize; 2]>,
    block_requests: Arc<std::sync::Mutex<Vec<(u32, u32)>>>,
    hold_after_first_block: Arc<std::sync::atomic::AtomicBool>,
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
        Self::start_with_piece_length(info_hash, payload, initial_bitfield, 16).await
    }

    async fn start_with_piece_length(
        info_hash: [u8; 20],
        payload: Arc<Vec<u8>>,
        initial_bitfield: u8,
        piece_length: u32,
    ) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind partial seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let upload_request = Arc::new(tokio::sync::Notify::new());
        let release_first_piece = Arc::new(tokio::sync::Notify::new());
        let release_tail = Arc::new(tokio::sync::Notify::new());
        let connection_count = Arc::new(AtomicUsize::new(0));
        let connection_count_task = Arc::clone(&connection_count);
        let piece_request_counts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
        let block_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold_after_first_block = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let uploaded_bytes = Arc::new(AtomicUsize::new(0));
        let peer_unchoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peer_interested = Arc::new(tokio::sync::Notify::new());
        let peer_not_interested = Arc::new(tokio::sync::Notify::new());
        let mut peer_id = [0x53; 20];
        peer_id[18..].copy_from_slice(&addr.port().to_be_bytes());
        let peer_state = Arc::new(PartialSeederState {
            peer_id,
            upload_request: Arc::clone(&upload_request),
            release_first_piece: Arc::clone(&release_first_piece),
            release_tail: Arc::clone(&release_tail),
            uploaded_bytes: Arc::clone(&uploaded_bytes),
            peer_unchoked: Arc::clone(&peer_unchoked),
            peer_interested: Arc::clone(&peer_interested),
            peer_not_interested: Arc::clone(&peer_not_interested),
            piece_request_counts: Arc::clone(&piece_request_counts),
            block_requests: Arc::clone(&block_requests),
            hold_after_first_block: Arc::clone(&hold_after_first_block),
        });
        let peer_state_task = Arc::clone(&peer_state);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        connection_count_task.fetch_add(1, Ordering::SeqCst);
                        connections.spawn(serve_partial_peer(
                            stream,
                            info_hash,
                            Arc::clone(&payload),
                            initial_bitfield,
                            Arc::clone(&peer_state_task),
                            piece_length,
                        ));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            connection_count,
            piece_request_counts,
            block_requests,
            hold_after_first_block,
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

    fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    fn piece_request_count(&self, piece: usize) -> usize {
        self.piece_request_counts[piece].load(Ordering::SeqCst)
    }

    fn block_request_count(&self, piece: u32, offset: u32) -> usize {
        self.block_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|&&request| request == (piece, offset))
            .count()
    }

    fn release_held_blocks(&self) {
        self.hold_after_first_block.store(false, Ordering::SeqCst);
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
    state: Arc<PartialSeederState>,
    piece_length: u32,
) {
    let upload_request = &state.upload_request;
    let release_first_piece = &state.release_first_piece;
    let release_tail = &state.release_tail;
    let uploaded_bytes = &state.uploaded_bytes;
    let peer_unchoked = &state.peer_unchoked;
    let peer_interested = &state.peer_interested;
    let peer_not_interested = &state.peer_not_interested;
    let piece_request_counts = &state.piece_request_counts;
    let block_requests = &state.block_requests;
    let hold_after_first_block = &state.hold_after_first_block;

    let mut request_handshake = [0u8; 68];
    if tokio::time::timeout(
        Duration::from_secs(3),
        stream.read_exact(&mut request_handshake),
    )
    .await
    .is_err()
        || request_handshake[0] != 19
        || request_handshake[28..48] != info_hash
    {
        return;
    }
    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[27] |= 0x04;
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(&state.peer_id);
    let bitfield = [0, 0, 0, 2, 5, initial_bitfield];
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&bitfield).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 2]).await.is_err()
    {
        return;
    }

    let mut upload_request_sent = false;
    let mut local_piece_available = false;
    let mut first_piece_released = initial_bitfield & 0x80 != 0;
    let mut tail_released = false;
    loop {
        let message = tokio::select! {
            message = read_peer_message_or_disconnect(&mut stream) => {
                let Some(message) = message else { return };
                Some(message)
            }
            _ = upload_request.notified(), if !upload_request_sent && local_piece_available => {
                if stream.write_all(&[0, 0, 0, 1, 2]).await.is_err() {
                    return;
                }
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
            Some(4) if message.len() == 5 => {
                let piece_index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                if piece_index == 0 {
                    local_piece_available = true;
                }
            }
            Some(5) if message.len() >= 2 => {
                local_piece_available = message[1] & 0x80 != 0;
            }
            Some(14) => local_piece_available = true,
            Some(15) => local_piece_available = false,
            Some(6) if message.len() == 13 => {
                let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                let begin = u32::from_be_bytes(message[5..9].try_into().unwrap());
                block_requests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((index, begin));
                if let Some(count) = piece_request_counts.get(index as usize) {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                if hold_after_first_block.load(Ordering::SeqCst) && index == 0 && begin != 0 {
                    continue;
                }
                let begin = begin as usize;
                let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                let start = index as usize * piece_length as usize + begin;
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
async fn cli_restores_bt_metadata_and_verified_pieces_across_process_restart() {
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
    let session_path = output_dir.path().join("bt-session.txt");
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

    let first_piece_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &first,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "16" {
            assert_eq!(status["status"], "active", "tail piece is still pending");
            assert_eq!(status["totalLength"], "32");
            break;
        }
        assert!(
            Instant::now() < first_piece_deadline,
            "first process did not verify one piece: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        peer.piece_request_count(0) > 0,
        "the first process must fetch piece 0 from the controlled peer"
    );
    let piece_zero_requests_before_restart = peer.piece_request_count(0);

    assert_eq!(
        rpc(&first, 3, "aria2.saveSession", json!([])),
        "OK",
        "saveSession must persist the active BT task"
    );
    let saved_session = std::fs::read_to_string(&session_path)
        .expect("saveSession writes the configured session file");
    assert!(
        saved_session.contains("aria2-rust-bt-metadata-data="),
        "session entry must carry the torrent metadata needed by its synthetic bt:// URI"
    );
    assert!(
        saved_session.contains(" BITFIELD=80\n"),
        "session entry must preserve the verified first-piece bitfield: {saved_session}"
    );

    let _ = rpc(&first, 4, "aria2.forceShutdown", json!([]));
    let exit = first.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "first aria2c process exits cleanly: {exit}");
    let saved_after_exit = std::fs::read_to_string(&session_path)
        .expect("shutdown must leave the configured session file readable");
    assert!(
        saved_after_exit.contains("aria2-rust-bt-metadata-data="),
        "shutdown must preserve torrent metadata when it rewrites the session: {saved_after_exit}"
    );
    assert!(
        saved_after_exit.contains(" BITFIELD=80\n"),
        "shutdown must preserve the verified-piece bitfield: {saved_after_exit}"
    );

    let mut second_args = common_args().to_vec();
    second_args.push(format!("--input-file={}", session_path.display()));
    let queries_before_restart = tracker.captured_queries().await.len();
    let second = RunningAria2::start_rpc(&second_args);
    assert!(
        tracker
            .wait_for_query_count(queries_before_restart + 1, Duration::from_secs(5))
            .await,
        "restored process must send its initial tracker announce"
    );
    let queries_after_restart = tracker.captured_queries().await;
    let resumed_started_announce = queries_after_restart[queries_before_restart..]
        .iter()
        .find(|query| query.contains("event=started"))
        .unwrap_or_else(|| {
            panic!(
                "restored process must send event=started; observed announces: {:?}",
                &queries_after_restart[queries_before_restart..]
            )
        })
        .as_str();
    let tracker_parameter = |query: &str, name: &str| {
        query
            .split_once('?')
            .map(|(_, query)| query)
            .into_iter()
            .flat_map(|query| query.split('&'))
            .filter_map(|parameter| parameter.split_once('='))
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_owned())
    };
    assert_eq!(
        tracker_parameter(resumed_started_announce, "downloaded"),
        Some("0".to_owned()),
        "tracker downloaded is the current process transfer count"
    );
    assert_eq!(
        tracker_parameter(resumed_started_announce, "left"),
        Some("16".to_owned()),
        "initial announce must report the 16 bytes remaining after piece restore: {resumed_started_announce}"
    );
    let restored_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &second,
            5,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "numPieces"]
            ]),
        );
        if status["status"] == "active" {
            assert_eq!(
                status["completedLength"], "16",
                "verified progress is restored"
            );
            assert_eq!(status["totalLength"], "32");
            assert_eq!(status["numPieces"], "2", "BT metadata is loaded at runtime");
            break;
        }
        assert!(
            Instant::now() < restored_deadline,
            "second process did not resume the saved BitTorrent task: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let reconnect_deadline = Instant::now() + Duration::from_secs(5);
    while peer.connection_count() < 2 {
        assert!(
            Instant::now() < reconnect_deadline,
            "restored process did not reconnect to the controlled peer"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        peer.piece_request_count(0),
        piece_zero_requests_before_restart,
        "the restored verified piece must not be downloaded again"
    );
}

#[tokio::test]
async fn cli_resumes_only_missing_piece_blocks_after_pause_and_restart() {
    const PIECE_LENGTH: usize = 32 * 1024;
    const BLOCK_LENGTH: usize = 16 * 1024;
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = Arc::new([vec![0x41; PIECE_LENGTH], vec![0x42; PIECE_LENGTH]].concat());
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder =
        two_piece_torrent_with_payload(&placeholder_tracker.announce_url(), &payload, PIECE_LENGTH);
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("multi-block torrent metadata parses");
    let peer = PartialSeeder::start_with_piece_length(
        meta.info_hash.bytes,
        Arc::clone(&payload),
        0x80,
        PIECE_LENGTH as u32,
    )
    .await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = two_piece_torrent_with_payload(&tracker.announce_url(), &payload, PIECE_LENGTH);
    let session_path = output_dir.path().join("partial-session.txt");
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

    let received_first_block_deadline = Instant::now() + Duration::from_secs(15);
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
            Instant::now() < received_first_block_deadline,
            "the first full block was not received before the pause: {peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        peer.block_request_count(0, 0) > 0,
        "the first block must have been requested"
    );
    let completed_block_request_count = peer.block_request_count(0, 0);

    assert_eq!(rpc(&first, 3, "aria2.pause", json!([gid])), gid);
    let pause_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rpc(&first, 4, "aria2.tellStatus", json!([gid, ["status"]]));
        if status["status"] == "paused" {
            break;
        }
        assert!(
            Instant::now() < pause_deadline,
            "task did not pause: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(rpc(&first, 5, "aria2.saveSession", json!([])), "OK");
    let _ = rpc(&first, 6, "aria2.forceShutdown", json!([]));
    let exit = first.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "first aria2c process exits cleanly: {exit}");

    let output_path = output_dir.path().join("active-upload.bin");
    let sidecar = aria2_core::filesystem::control_file::ControlFile::load(
        &aria2_core::filesystem::control_file::ControlFile::control_path_for(&output_path),
    )
    .await
    .expect("read paused torrent control file")
    .expect("paused torrent has a control file");
    assert_eq!(
        sidecar
            .in_flight_pieces()
            .iter()
            .find(|piece| piece.index == 0)
            .map(|piece| piece.bitfield.as_slice()),
        Some([0x80].as_slice()),
        "the payload is flushed before the first block's in-flight bit is saved"
    );

    peer.release_held_blocks();
    let mut second_args = common_args().to_vec();
    second_args.push(format!("--input-file={}", session_path.display()));
    let mut second = RunningAria2::start_rpc(&second_args);
    assert!(
        tracker
            .wait_for_query_count(2, Duration::from_secs(5))
            .await,
        "restored process must announce to the tracker; queries: {:?}",
        tracker.captured_queries().await
    );
    let reconnect_deadline = Instant::now() + Duration::from_secs(5);
    while peer.connection_count() < 2 {
        assert!(
            Instant::now() < reconnect_deadline,
            "restored process announced but did not reconnect to the peer"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let restored_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &second,
            7,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["status"] == "active" {
            break;
        }
        if status["status"] == "paused" {
            let _ = rpc(&second, 8, "aria2.unpause", json!([gid]));
        }
        assert!(
            Instant::now() < restored_deadline,
            "second process did not restore the paused task: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let piece_complete_deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = rpc(
            &second,
            9,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"]
            .as_str()
            .and_then(|completed| completed.parse::<usize>().ok())
            == Some(PIECE_LENGTH)
        {
            assert_eq!(
                status["status"], "active",
                "the second piece remains unavailable"
            );
            assert_eq!(status["totalLength"], (PIECE_LENGTH * 2).to_string());
            break;
        }
        if Instant::now() >= piece_complete_deadline {
            let requests = peer
                .block_requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let peer_details = rpc(&second, 20, "aria2.getPeerDetails", json!([gid]));
            panic!(
                "restored piece did not verify after downloading the missing block: {status}; connections={}, requests={requests:?}, peers={peer_details}, tracker={:?}",
                peer.connection_count(),
                tracker.captured_queries().await
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        peer.block_request_count(0, 0),
        completed_block_request_count,
        "restart must not request the already persisted first block"
    );
    assert!(
        peer.block_request_count(0, BLOCK_LENGTH as u32) > 0,
        "restart must request the missing second block"
    );

    let _ = rpc(&second, 10, "aria2.forceShutdown", json!([]));
    let exit = second.wait_for_exit(Duration::from_secs(10));
    assert!(
        exit.success(),
        "second aria2c process exits cleanly: {exit}"
    );

    let sidecar = aria2_core::filesystem::control_file::ControlFile::load(
        &aria2_core::filesystem::control_file::ControlFile::control_path_for(&output_path),
    )
    .await
    .expect("read verified piece control file")
    .expect("verified piece has a control file");
    assert_eq!(
        sidecar.bitfield().first().copied(),
        Some(0x80),
        "the completed piece must be persisted in the sidecar; completed={}, in-flight={:?}",
        sidecar.completed_length(),
        sidecar.in_flight_pieces()
    );
    assert!(
        !sidecar
            .in_flight_pieces()
            .iter()
            .any(|piece| piece.index == 0),
        "a verified piece must not remain marked in flight"
    );

    let downloaded = std::fs::read(&output_path).expect("resumed output is readable");
    let first_piece = &downloaded[..PIECE_LENGTH];
    let expected_first_piece = &payload[..PIECE_LENGTH];
    let first_mismatch = first_piece
        .iter()
        .zip(expected_first_piece)
        .position(|(actual, expected)| actual != expected);
    assert!(
        first_mismatch.is_none(),
        "resumed piece differs at byte {:?}: actual={:?}, expected={:?}, output length={}",
        first_mismatch,
        first_mismatch.map(|index| first_piece[index]),
        first_mismatch.map(|index| expected_first_piece[index]),
        downloaded.len()
    );
}

#[tokio::test]
async fn cli_magnet_persists_resolved_metadata_for_process_restart() {
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
    let source_path = output_dir.path().join("exact-source.torrent");
    std::fs::write(&source_path, &torrent).expect("write initial magnet exact source");
    let source_path = source_path.to_string_lossy().replace('\\', "/");
    let source_uri = if cfg!(windows) {
        format!("file:///{}", source_path.replace(' ', "%20"))
    } else {
        format!("file://{}", source_path.replace(' ', "%20"))
    };
    let magnet_uri = format!(
        "magnet:?xt=urn:btih:{}&xs={source_uri}",
        meta.info_hash.as_hex()
    );
    let session_path = output_dir.path().join("magnet-session.txt");
    let listen_port = reserve_loopback_port();
    let common_args = || {
        [
            format!("--dir={}", output_dir.path().display()),
            format!("--listen-port={listen_port}"),
            "--enable-dht=false".to_owned(),
            "--enable-public-trackers=false".to_owned(),
            "--enable-peer-exchange=false".to_owned(),
            "--bt-enable-web-seed=false".to_owned(),
            "--bt-save-metadata=false".to_owned(),
            "--bt-load-saved-metadata=false".to_owned(),
            "--seed-time=3600".to_owned(),
            format!("--save-session={}", session_path.display()),
        ]
    };

    let mut first = RunningAria2::start_rpc(&common_args());
    let gid = rpc(&first, 1, "aria2.addUri", json!([[magnet_uri], {}]))
        .as_str()
        .expect("addUri returns a GID")
        .to_owned();

    let first_piece_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &first,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "16" {
            assert_eq!(status["status"], "active", "tail piece is still pending");
            assert_eq!(status["totalLength"], "32");
            break;
        }
        assert!(
            Instant::now() < first_piece_deadline,
            "magnet did not resolve metadata and verify its first piece: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(peer.piece_request_count(0) > 0);
    let piece_zero_requests_before_restart = peer.piece_request_count(0);

    assert_eq!(
        rpc(&first, 3, "aria2.saveSession", json!([])),
        "OK",
        "saveSession must persist the active magnet task"
    );
    let saved_session = std::fs::read_to_string(&session_path)
        .expect("saveSession writes the configured session file");
    assert!(
        saved_session.contains("aria2-rust-bt-metadata-data="),
        "resolved magnet metadata must be embedded in the resumable session: {saved_session}"
    );

    let _ = rpc(&first, 4, "aria2.forceShutdown", json!([]));
    let exit = first.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "first aria2c process exits cleanly: {exit}");
    std::fs::remove_file(output_dir.path().join("exact-source.torrent"))
        .expect("remove the only metadata source before restart");

    let mut second_args = common_args().to_vec();
    second_args.push(format!("--input-file={}", session_path.display()));
    let mut second = RunningAria2::start_rpc(&second_args);
    let restored_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = rpc(
            &second,
            5,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "numPieces"]
            ]),
        );
        if status["status"] == "active" {
            assert_eq!(status["completedLength"], "16");
            assert_eq!(status["totalLength"], "32");
            assert_eq!(status["numPieces"], "2");
            break;
        }
        assert!(
            Instant::now() < restored_deadline,
            "magnet task did not resume from the session after its exact source was removed: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let reconnect_deadline = Instant::now() + Duration::from_secs(5);
    while peer.connection_count() < 2 {
        assert!(
            Instant::now() < reconnect_deadline,
            "restored magnet task did not reconnect to the controlled peer"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        peer.piece_request_count(0),
        piece_zero_requests_before_restart,
        "the restored magnet task must not download its verified piece again"
    );
    let _ = rpc(&second, 6, "aria2.forceShutdown", json!([]));
    let exit = second.wait_for_exit(Duration::from_secs(10));
    assert!(
        exit.success(),
        "second aria2c process exits cleanly: {exit}"
    );
}

#[tokio::test]
async fn cli_magnet_reuses_metadata_peer_actor_for_payload_download() {
    let output_dir = tempfile::tempdir().expect("temporary magnet output directory");
    let placeholder = test_torrent("http://127.0.0.1:1/announce");
    let (metadata, info_bytes) =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse_with_info_bytes(
            &placeholder,
        )
        .expect("placeholder torrent metadata parses");
    let peer = MockBtPeerServer::start_with_metadata(
        metadata.info_hash.bytes,
        vec![b"abc".to_vec()],
        Some(info_bytes),
    )
    .await;
    let tracker = MockTrackerServer::start(peer.addr().port()).await;
    let encoded_tracker = tracker
        .announce_url()
        .replace(':', "%3A")
        .replace('/', "%2F");
    let magnet_uri = format!(
        "magnet:?xt=urn:btih:{}&tr={encoded_tracker}",
        metadata.info_hash.as_hex()
    );
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-dht6=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-metadata-only=false".to_owned(),
        "--bt-tracker-timeout=3".to_owned(),
        "--bt-tracker-connect-timeout=2".to_owned(),
        "--max-tries=1".to_owned(),
        "--seed-time=30".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let gid = rpc(&client, 1, "aria2.addUri", json!([[magnet_uri], {}]))
        .as_str()
        .expect("addUri returns a GID")
        .to_owned();

    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "3" {
            assert_eq!(
                status["status"], "active",
                "torrent should be in its seed phase"
            );
            assert_eq!(status["totalLength"], "3");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "magnet should resolve BEP 9 metadata and download its payload: status={status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        peer.wait_for_handshakes(1, Duration::from_secs(1)).await,
        "the metadata peer must complete its initial handshake"
    );
    assert_eq!(
        peer.completed_handshake_count(),
        1,
        "payload transfer must reuse the metadata-phase connection, not reconnect"
    );
    assert!(
        peer.requested_pieces().await.contains(&0),
        "the retained peer actor must receive the payload piece request"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("actor-runtime.bin"))
            .expect("magnet payload is written"),
        b"abc"
    );
}

#[tokio::test]
async fn cli_magnet_retries_metadata_on_another_peer_after_extension_revocation() {
    let output_dir = tempfile::tempdir().expect("temporary magnet output directory");
    let placeholder = test_torrent("http://127.0.0.1:1/announce");
    let (metadata, info_bytes) =
        aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse_with_info_bytes(
            &placeholder,
        )
        .expect("placeholder torrent metadata parses");
    let revoked_peer = MockBtPeerServer::start_revoking_metadata_after_first_request(
        metadata.info_hash.bytes,
        vec![b"abc".to_vec()],
        info_bytes.clone(),
    )
    .await;
    let replacement_peer = MockBtPeerServer::start_with_delayed_metadata_handshake(
        metadata.info_hash.bytes,
        vec![b"abc".to_vec()],
        info_bytes,
        Duration::from_millis(500),
    )
    .await;
    let tracker = MockTrackerServer::start_with_peers(
        vec![revoked_peer.addr().port(), replacement_peer.addr().port()],
        false,
    )
    .await;
    let encoded_tracker = tracker
        .announce_url()
        .replace(':', "%3A")
        .replace('/', "%2F");
    let magnet_uri = format!(
        "magnet:?xt=urn:btih:{}&tr={encoded_tracker}",
        metadata.info_hash.as_hex()
    );
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-dht6=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-metadata-only=false".to_owned(),
        "--bt-tracker-timeout=3".to_owned(),
        "--bt-tracker-connect-timeout=2".to_owned(),
        "--max-tries=1".to_owned(),
        "--seed-time=30".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let gid = rpc(&client, 1, "aria2.addUri", json!([[magnet_uri], {}]))
        .as_str()
        .expect("addUri returns a GID")
        .to_owned();

    assert!(
        revoked_peer
            .wait_for_metadata_requests(1, Duration::from_secs(5))
            .await,
        "the first peer should receive a BEP 9 metadata request before revoking ut_metadata"
    );
    assert!(
        replacement_peer
            .wait_for_metadata_requests(1, Duration::from_secs(5))
            .await,
        "the scheduler should move the outstanding metadata request to the replacement peer"
    );

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "3" {
            assert_eq!(status["status"], "active");
            assert_eq!(status["totalLength"], "3");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "magnet must finish metadata and payload after ut_metadata revocation: {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(
        revoked_peer.metadata_request_count(),
        1,
        "a peer that revoked ut_metadata must not receive another metadata request"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("actor-runtime.bin"))
            .expect("replacement peer payload is written"),
        b"abc"
    );
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

    tokio::time::sleep(Duration::from_millis(650)).await;
    let sustained_upload_status = rpc(
        &client,
        31,
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
    assert_eq!(sustained_upload_status["status"], "active");
    assert_eq!(sustained_upload_status["completedLength"], "16");
    assert_eq!(sustained_upload_status["totalLength"], "32");
    assert_eq!(sustained_upload_status["uploadLength"], "16");
    assert!(
        sustained_upload_status["uploadSpeed"]
            .as_str()
            .and_then(|speed| speed.parse::<u64>().ok())
            .is_some_and(|speed| speed > 0),
        "recent upload must remain visible in the 10-second task rate window: {sustained_upload_status}"
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
    let seed_pair_disconnect_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let peer_details = rpc(&client, 5, "aria2.getPeerDetails", json!([gid]));
        if peer_details.as_array().is_some_and(Vec::is_empty) {
            break;
        }
        assert!(
            Instant::now() < seed_pair_disconnect_deadline,
            "a seed-to-seed peer should leave the active RPC snapshot after torrent completion: {peer_details}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        std::fs::read(output_dir.path().join("active-upload.bin")).unwrap(),
        *payload
    );
}

#[tokio::test]
async fn cli_upload_speed_uses_one_torrent_wide_sample_window() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = two_piece_upload_torrent(&placeholder_tracker.announce_url());
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("two-piece torrent metadata parses");
    let payload = Arc::new([vec![0x41; 16], vec![0x42; 16]].concat());
    let peers = [
        PartialSeeder::start_with_bitfield(meta.info_hash.bytes, Arc::clone(&payload), 0x80).await,
        PartialSeeder::start_with_bitfield(meta.info_hash.bytes, Arc::clone(&payload), 0x80).await,
        PartialSeeder::start_with_bitfield(meta.info_hash.bytes, Arc::clone(&payload), 0x80).await,
    ];
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start_with_peers(
        peers.iter().map(|peer| peer.addr.port()).collect(),
        false,
    )
    .await;
    let torrent = two_piece_upload_torrent(&tracker.announce_url());
    std::fs::write(
        output_dir.path().join("active-upload.bin"),
        payload.as_slice(),
    )
    .expect("preseed the fully verified torrent payload");
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--check-integrity=true".to_owned(),
        "--bt-hash-check-seed=true".to_owned(),
        "--seed-time=60".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let mut client = RunningAria2::start_rpc(&args);
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

    let seed_deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([gid, ["completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "32" {
            break;
        }
        assert!(
            Instant::now() < seed_deadline,
            "preverified torrent did not enter seeding: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        tracker
            .wait_for_query_count(1, Duration::from_secs(5))
            .await,
        "seeding task did not announce and discover upload peers"
    );

    let peers_ready_deadline = Instant::now() + Duration::from_secs(8);
    while peers
        .iter()
        .any(|peer| peer.connection_count() == 0 || !peer.peer_unchoked.load(Ordering::SeqCst))
    {
        assert!(
            Instant::now() < peers_ready_deadline,
            "the seeding actor did not establish and unchoke all three loopback peers"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    const UPLOAD_BYTES: usize = 16;
    let first_upload_started = Instant::now();
    peers[0].request_upload();
    wait_for_uploaded_bytes(&peers[0], UPLOAD_BYTES).await;
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        first_upload_started + Duration::from_millis(850),
    ))
    .await;
    peers[1].request_upload();
    wait_for_uploaded_bytes(&peers[1], UPLOAD_BYTES).await;
    tokio::time::sleep_until(tokio::time::Instant::from_std(
        first_upload_started + Duration::from_millis(1650),
    ))
    .await;
    peers[2].request_upload();
    wait_for_uploaded_bytes(&peers[2], UPLOAD_BYTES).await;

    tokio::time::sleep_until(tokio::time::Instant::from_std(
        first_upload_started + Duration::from_millis(10_200),
    ))
    .await;
    let status = rpc(
        &client,
        3,
        "aria2.tellStatus",
        json!([
            gid,
            ["status", "completedLength", "uploadLength", "uploadSpeed"]
        ]),
    );
    assert_eq!(status["status"], "active");
    assert_eq!(status["completedLength"], "32");
    assert_eq!(status["uploadLength"], "48");
    let upload_speed = status["uploadSpeed"]
        .as_str()
        .and_then(|speed| speed.parse::<u64>().ok())
        .expect("RPC uploadSpeed is an integer");
    assert!(
        (1..=2).contains(&upload_speed),
        "after the first global one-second slot expires, only the last 16-byte sample remains; expected 1-2 B/s, got {upload_speed}: {status}"
    );

    let _ = rpc(&client, 4, "aria2.forceShutdown", json!([]));
    let exit = client.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "aria2c exits cleanly: {exit}");
}

async fn wait_for_uploaded_bytes(peer: &PartialSeeder, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while peer.uploaded_bytes.load(Ordering::SeqCst) < expected {
        assert!(
            Instant::now() < deadline,
            "loopback peer did not receive the requested upload"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
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
    let log_path = output_dir.path().join("aria2.log");
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--log={}", log_path.display()),
        "--log-level=debug".to_owned(),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-max-peers=1".to_owned(),
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

    let change_result = rpc(
        &client,
        20,
        "aria2.changeOption",
        json!([gid, {"bt-max-peers": "2"}]),
    );
    assert_eq!(change_result, "OK");
    let options = rpc(&client, 21, "aria2.getOption", json!([gid]));
    assert_eq!(options["bt-max-peers"], "2");

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
    let bitfield = tokio::time::timeout(
        Duration::from_secs(2),
        read_peer_message_or_disconnect(&mut leecher),
    )
    .await;
    let bitfield = match bitfield {
        Ok(Some(bitfield)) => bitfield,
        Ok(None) => panic!(
            "incoming actor disconnected before advertising its piece bitfield; aria2 log: {}",
            std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| format!("unavailable: {error}"))
        ),
        Err(_) => panic!(
            "incoming actor did not advertise its piece bitfield before timeout; aria2 log: {}",
            std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| format!("unavailable: {error}"))
        ),
    };
    assert_eq!(
        bitfield,
        [5, 0x80],
        "only the verified first piece is available"
    );
    leecher
        .write_all(&[0, 0, 0, 1, 2])
        .await
        .expect("express interest only after learning piece availability");

    let unchoke_result = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let Some(message) = read_peer_message_or_disconnect(&mut leecher).await else {
                break false;
            };
            if message.first() == Some(&1) {
                break true;
            }
        }
    })
    .await;
    match unchoke_result {
        Ok(true) => {}
        Ok(false) => panic!(
            "incoming actor disconnected before unchoking the interested peer; aria2 log: {}",
            std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| format!("unavailable: {error}"))
        ),
        Err(_) => panic!(
            "incoming peer was not unchoked while a piece download was in flight; aria2 log: {}",
            std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| format!("unavailable: {error}"))
        ),
    }

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
    let piece_result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let Some(message) = read_peer_message_or_disconnect(&mut leecher).await else {
                break None;
            };
            if message.first() == Some(&7) {
                break Some(message);
            }
        }
    })
    .await;
    let piece = match piece_result {
        Ok(Some(piece)) => piece,
        Ok(None) => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let status = rpc(
                &client,
                10,
                "aria2.tellStatus",
                json!([
                    gid,
                    ["status", "completedLength", "totalLength", "numSeeders"]
                ]),
            );
            let peers = rpc(&client, 11, "aria2.getPeers", json!([gid]));
            let peer_details = rpc(&client, 12, "aria2.getPeerDetails", json!([gid]));
            panic!(
                "incoming actor disconnected while handling a piece request; status={status}; peers={peers}; peer_details={peer_details}; aria2 log: {}",
                std::fs::read_to_string(&log_path)
                    .unwrap_or_else(|error| format!("unavailable: {error}"))
            )
        }
        Err(_) => panic!(
            "incoming actor did not upload the verified piece before timeout; aria2 log: {}",
            std::fs::read_to_string(&log_path)
                .unwrap_or_else(|error| format!("unavailable: {error}"))
        ),
    };
    assert_eq!(&piece[1..5], &0u32.to_be_bytes());
    assert_eq!(&piece[5..9], &0u32.to_be_bytes());
    assert_eq!(&piece[9..], &payload[..16]);

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
    assert_eq!(active_status["uploadLength"], "16");
    assert!(
        active_status["uploadSpeed"]
            .as_str()
            .and_then(|speed| speed.parse::<u64>().ok())
            .is_some_and(|speed| speed > 0),
        "the torrent-wide upload window must report the in-flight peer payload: {active_status}"
    );
    let peer_details_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let details = rpc(&client, 4, "aria2.getPeerDetails", json!([gid]));
        if details.as_array().is_some_and(|peers| {
            peers.iter().any(|peer| {
                peer["source"] == "incoming"
                    && peer["uploadedBytes"] == "16"
                    && peer["flags"]["peerInterested"] == true
                    && peer["flags"]["amChoking"] == false
                    && peer["outstandingRequestsFromPeer"] == 0
            })
        }) {
            break;
        }
        assert!(
            Instant::now() < peer_details_deadline,
            "peer-details did not publish the incoming leecher's upload and protocol state within the bounded snapshot interval: {details}"
        );
        tokio::task::yield_now().await;
    }

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
    let completed_status = rpc(
        &client,
        5,
        "aria2.tellStatus",
        json!([gid, ["status", "uploadLength", "uploadSpeed"]]),
    );
    assert_eq!(completed_status["uploadLength"], "16");
    assert_eq!(
        completed_status["uploadSpeed"], "0",
        "the completed snapshot must zero speeds even though the active-download snapshot recorded the recent upload: {completed_status}"
    );
    assert_eq!(
        std::fs::read(output_dir.path().join("active-upload.bin")).unwrap(),
        *payload
    );
}
