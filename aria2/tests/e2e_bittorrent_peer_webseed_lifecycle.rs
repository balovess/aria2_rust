#![cfg(feature = "bittorrent")]

//! CLI/RPC regression coverage for peer lifecycle while a WebSeed is pending.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, oneshot};
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

fn torrent_with_web_seed(tracker_url: &str, web_seed_url: &str) -> Vec<u8> {
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
        BencodeValue::Bytes(web_seed_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

struct NoPiecePeer {
    addr: SocketAddr,
    disconnect: Arc<Notify>,
    connected: Option<oneshot::Receiver<()>>,
    task: JoinHandle<()>,
}

impl NoPiecePeer {
    async fn start(info_hash: [u8; 20]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind no-piece peer");
        let addr = listener.local_addr().expect("peer address");
        let disconnect = Arc::new(Notify::new());
        let peer_disconnect = Arc::clone(&disconnect);
        let (connected_tx, connected) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut stream = loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut prefix = [0u8; 1];
                if tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut prefix))
                    .await
                    .is_err()
                    || prefix[0] != 19
                {
                    continue;
                }
                let mut request = [0u8; 68];
                request[0] = prefix[0];
                if stream.read_exact(&mut request[1..]).await.is_err()
                    || request[28..48] != info_hash
                {
                    continue;
                }

                let mut response = [0u8; 68];
                response[0] = 19;
                response[1..20].copy_from_slice(b"BitTorrent protocol");
                response[28..48].copy_from_slice(&info_hash);
                response[48..68].copy_from_slice(b"ActorSeeder-00000001");
                if stream.write_all(&response).await.is_err()
                    || stream.write_all(&[0, 0, 0, 2, 5, 0]).await.is_err()
                {
                    continue;
                }
                break stream;
            };
            let _ = connected_tx.send(());

            let close = peer_disconnect.notified();
            tokio::pin!(close);
            loop {
                tokio::select! {
                    _ = &mut close => {
                        let _ = stream.shutdown().await;
                        return;
                    }
                    result = read_peer_message(&mut stream) => {
                        if result.is_err() {
                            return;
                        }
                    }
                }
            }
        });
        Self {
            addr,
            disconnect,
            connected: Some(connected),
            task,
        }
    }

    async fn wait_connected(&mut self) {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.connected
                .take()
                .expect("peer handshake wait is one-shot"),
        )
        .await
        .expect("CLI should connect to the no-piece peer")
        .expect("peer fixture should complete the handshake");
    }

    fn disconnect(&self) {
        self.disconnect.notify_one();
    }
}

impl Drop for NoPiecePeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_peer_message(stream: &mut TcpStream) -> std::io::Result<()> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_be_bytes(length) as usize;
    if length > 0 {
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).await?;
    }
    Ok(())
}

struct PendingWebSeed {
    addr: SocketAddr,
    requests: Arc<AtomicUsize>,
    request_seen: Arc<Notify>,
    task: JoinHandle<()>,
}

impl PendingWebSeed {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind pending WebSeed");
        let addr = listener.local_addr().expect("WebSeed address");
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::clone(&requests);
        let request_seen = Arc::new(Notify::new());
        let request_notification = Arc::clone(&request_seen);
        let task = tokio::spawn(async move {
            let mut handlers = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { return };
                        let requests = Arc::clone(&request_count);
                        let request_seen = Arc::clone(&request_notification);
                        handlers.spawn(async move {
                            let mut request = Vec::new();
                            let mut buffer = [0u8; 512];
                            loop {
                                let count = stream.read(&mut buffer).await?;
                                if count == 0 {
                                    return Ok::<(), std::io::Error>(());
                                }
                                request.extend_from_slice(&buffer[..count]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    requests.fetch_add(1, Ordering::SeqCst);
                                    request_seen.notify_one();
                                    std::future::pending::<()>().await;
                                }
                            }
                        });
                    }
                    Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            requests,
            request_seen,
            task,
        }
    }

    async fn wait_for_request(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if self.requests.load(Ordering::SeqCst) > 0 {
                    return;
                }
                self.request_seen.notified().await;
            }
        })
        .await
        .expect("piece scheduler should issue a WebSeed range request");
    }
}

impl Drop for PendingWebSeed {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn cli_removes_disconnected_peer_while_webseed_request_is_pending() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder =
        torrent_with_web_seed(&placeholder_tracker.announce_url(), "http://127.0.0.1/file");
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("torrent metadata parses");
    let mut peer = NoPiecePeer::start(meta.info_hash.bytes).await;
    let web_seed = PendingWebSeed::start().await;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start(peer.addr.port()).await;
    let torrent = torrent_with_web_seed(
        &tracker.announce_url(),
        &format!("http://{}/file.iso", web_seed.addr),
    );
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!(
            "--listen-port={}",
            TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        ),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=true".to_owned(),
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

    peer.wait_connected().await;
    web_seed.wait_for_request().await;

    let initial_peers = rpc(&client, 2, "aria2.getPeerDetails", json!([gid]));
    assert_eq!(initial_peers.as_array().map(Vec::len), Some(1));
    assert_eq!(initial_peers[0]["bitfield"], "00");

    peer.disconnect();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let peers = rpc(&client, 3, "aria2.getPeerDetails", json!([gid]));
        if peers.as_array().is_some_and(Vec::is_empty) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "a disconnected actor must be removed while WebSeed I/O is pending: {peers}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let _ = rpc(&client, 4, "aria2.forceRemove", json!([gid]));
}
