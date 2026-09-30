#![cfg(feature = "bittorrent")]

//! Process-level regression coverage for upload accounting across the
//! download-to-seeding actor lifecycle.

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
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

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
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve a loopback port");
    listener.local_addr().expect("reserved address").port()
}

fn torrent_bytes(tracker_url: &str, payload: &[u8]) -> Vec<u8> {
    assert_eq!(payload, b"seed", "fixed piece hash is for the seed payload");
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(payload.len() as i64));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"seed-counter.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16 * 1024));
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(vec![
            0x92, 0x71, 0x3d, 0x47, 0x09, 0x37, 0x71, 0x11, 0xcf, 0x31, 0xf2, 0xa7, 0x19, 0x86,
            0xc4, 0x11, 0xbd, 0x6c, 0xb5, 0xb0,
        ]),
    );

    let mut torrent = BTreeMap::new();
    torrent.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    torrent.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(torrent).encode()
}

struct UploadPeer {
    addr: SocketAddr,
    request_upload: Arc<Notify>,
    uploaded_bytes: Arc<AtomicUsize>,
    accepted_connections: Arc<AtomicUsize>,
    accepted_handshakes: Arc<AtomicUsize>,
    observed_handshake: Arc<StdMutex<Vec<u8>>>,
    download_requests: Arc<AtomicUsize>,
    download_request_length: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl UploadPeer {
    async fn start(info_hash: [u8; 20], payload: Arc<Vec<u8>>) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind controlled BitTorrent peer");
        let addr = listener.local_addr().expect("peer address");
        let request_upload = Arc::new(Notify::new());
        let uploaded_bytes = Arc::new(AtomicUsize::new(0));
        let download_requests = Arc::new(AtomicUsize::new(0));
        let download_request_length = Arc::new(AtomicUsize::new(0));
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let accepted_connections_task = Arc::clone(&accepted_connections);
        let accepted_handshakes = Arc::new(AtomicUsize::new(0));
        let observed_handshake = Arc::new(StdMutex::new(Vec::new()));
        let session = PeerSession {
            info_hash,
            payload: Arc::clone(&payload),
            request_upload: Arc::clone(&request_upload),
            uploaded_bytes: Arc::clone(&uploaded_bytes),
            accepted_handshakes: Arc::clone(&accepted_handshakes),
            observed_handshake: Arc::clone(&observed_handshake),
            download_requests: Arc::clone(&download_requests),
            download_request_length: Arc::clone(&download_request_length),
        };
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                accepted_connections_task.fetch_add(1, Ordering::SeqCst);
                if serve_peer(stream, &session).await {
                    break;
                }
            }
        });

        Self {
            addr,
            request_upload,
            uploaded_bytes,
            accepted_connections,
            accepted_handshakes,
            observed_handshake,
            download_requests,
            download_request_length,
            task,
        }
    }
}

impl Drop for UploadPeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_bt_message(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
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

struct PeerSession {
    info_hash: [u8; 20],
    payload: Arc<Vec<u8>>,
    request_upload: Arc<Notify>,
    uploaded_bytes: Arc<AtomicUsize>,
    accepted_handshakes: Arc<AtomicUsize>,
    observed_handshake: Arc<StdMutex<Vec<u8>>>,
    download_requests: Arc<AtomicUsize>,
    download_request_length: Arc<AtomicUsize>,
}

async fn serve_peer(mut stream: TcpStream, peer: &PeerSession) -> bool {
    let mut client_handshake = [0u8; 68];
    if tokio::time::timeout(
        Duration::from_secs(5),
        stream.read_exact(&mut client_handshake),
    )
    .await
    .is_err()
        || client_handshake[0] != 19
        || &client_handshake[1..20] != b"BitTorrent protocol"
        || client_handshake[28..48] != peer.info_hash
    {
        *peer
            .observed_handshake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = client_handshake.to_vec();
        return false;
    }
    *peer
        .observed_handshake
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = client_handshake.to_vec();
    peer.accepted_handshakes.fetch_add(1, Ordering::SeqCst);

    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&peer.info_hash);
    handshake[48..68].copy_from_slice(b"SeedLifecyclePeer001");
    if stream.write_all(&handshake).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
    {
        return true;
    }

    let mut upload_request_sent = false;
    let mut upload_block_requested = false;
    loop {
        tokio::select! {
            message = read_bt_message(&mut stream) => {
                let Ok(message) = message else { return true };
                match message.first() {
                    Some(6) if message.len() == 13 => {
                        peer.download_requests.fetch_add(1, Ordering::SeqCst);
                        let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
                        let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                        peer.download_request_length.store(length, Ordering::SeqCst);
                        let Some(end) = begin.checked_add(length) else { return true };
                        if index != 0 || end > peer.payload.len() {
                            return true;
                        }
                        let mut piece = Vec::with_capacity(13 + length);
                        piece.extend_from_slice(&((9 + length) as u32).to_be_bytes());
                        piece.push(7);
                        piece.extend_from_slice(&index.to_be_bytes());
                        piece.extend_from_slice(&(begin as u32).to_be_bytes());
                        piece.extend_from_slice(&peer.payload[begin..end]);
                        if stream.write_all(&piece).await.is_err() {
                            return true;
                        }
                    }
                    Some(1) if upload_request_sent && !upload_block_requested => {
                        let mut request = Vec::with_capacity(17);
                        request.extend_from_slice(&13u32.to_be_bytes());
                        request.push(6);
                        request.extend_from_slice(&0u32.to_be_bytes());
                        request.extend_from_slice(&0u32.to_be_bytes());
                        request.extend_from_slice(&(peer.payload.len() as u32).to_be_bytes());
                        if stream.write_all(&request).await.is_err() {
                            return true;
                        }
                        upload_block_requested = true;
                    }
                    Some(7) if upload_block_requested => {
                        if message.len() < 9 {
                            return true;
                        }
                        let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap());
                        if index != 0 || begin != 0 || message.len() != 9 + peer.payload.len() {
                            return true;
                        }
                        peer.uploaded_bytes
                            .fetch_add(message.len() - 9, Ordering::SeqCst);
                        return true;
                    }
                    _ => {}
                }
            }
            _ = peer.request_upload.notified(), if !upload_request_sent => {
                if stream.write_all(&[0, 0, 0, 1, 2]).await.is_err() {
                    return true;
                }
                upload_request_sent = true;
            }
        }
    }
}

#[tokio::test]
async fn retained_peer_upload_counts_toward_seed_ratio_and_tracker_stop() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = Arc::new(b"seed".to_vec());
    let torrent_meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(
        &torrent_bytes("http://127.0.0.1/placeholder", &payload),
    )
    .expect("generated torrent parses");
    let peer = UploadPeer::start(torrent_meta.info_hash.bytes, Arc::clone(&payload)).await;
    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = torrent_bytes(&tracker.announce_url(), &payload);
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--log={}", output_dir.path().join("aria2.log").display()),
        "--log-level=debug".to_owned(),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-utp=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
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

    tracker.wait_for_event("started").await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "errorMessage"]
            ]),
        );
        if status["completedLength"] == "4" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "test torrent failed to download: status={status}, peers={}, trackers={}, requests={}, request_length={}, accepted={}, handshakes={}, handshake={:02x?}, log={}",
            rpc(&client, 3, "aria2.getPeers", json!([gid])),
            rpc(&client, 4, "aria2.getTrackers", json!([gid])),
            peer.download_requests.load(Ordering::SeqCst),
            peer.download_request_length.load(Ordering::SeqCst),
            peer.accepted_connections.load(Ordering::SeqCst),
            peer.accepted_handshakes.load(Ordering::SeqCst),
            peer.observed_handshake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            std::fs::read_to_string(output_dir.path().join("aria2.log"))
                .unwrap_or_else(|error| format!("<log unavailable: {error}>")),
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tracker.wait_for_event("completed").await;
    peer.request_upload.notify_one();

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if peer.uploaded_bytes.load(Ordering::SeqCst) == payload.len() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("retained peer actor did not upload the verified piece");

    tracker.wait_for_event("stopped").await;
    let queries = tracker.captured_queries().await;
    assert!(
        queries
            .iter()
            .any(|query| query.contains("event=stopped") && query.contains("uploaded=4")),
        "seeding stop must report the retained actor's upload in tracker uploaded stats: {queries:?}"
    );

    let status = rpc(
        &client,
        2,
        "aria2.tellStatus",
        json!([gid, ["status", "uploadLength", "completedLength"]]),
    );
    assert_eq!(status["completedLength"], "4");
    assert_eq!(status["uploadLength"], "4");
}
