#![cfg(feature = "bittorrent")]

//! Seed peers already confirmed complete must not be repeatedly redialed.

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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
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

fn one_piece_torrent(tracker_url: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(3));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"seed-peer-reuse.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16 * 1024));
    // SHA-1("abc"), independently fixed as this fixture's expected piece hash.
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(vec![
            0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
            0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d,
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

struct FullSeeder {
    addr: SocketAddr,
    handshakes: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

impl FullSeeder {
    async fn start(info_hash: [u8; 20]) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind full seeder fixture");
        let addr = listener.local_addr().expect("seeder address");
        let handshakes = Arc::new(AtomicUsize::new(0));
        let handshakes_task = Arc::clone(&handshakes);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        let handshakes = Arc::clone(&handshakes_task);
                        connections.spawn(serve_full_seed(stream, info_hash, handshakes));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            handshakes,
            task,
        }
    }

    fn handshake_count(&self) -> usize {
        self.handshakes.load(Ordering::SeqCst)
    }
}

impl Drop for FullSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_full_seed(mut stream: TcpStream, info_hash: [u8; 20], handshakes: Arc<AtomicUsize>) {
    let mut request = [0u8; 68];
    if tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut request))
        .await
        .is_err()
        || request[0] != 19
        || &request[1..20] != b"BitTorrent protocol"
        || request[28..48] != info_hash
    {
        return;
    }
    handshakes.fetch_add(1, Ordering::SeqCst);

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[27] |= 0x04;
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(&[0x5a; 20]);
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
    {
        return;
    }

    loop {
        let mut length = [0u8; 4];
        if tokio::time::timeout(Duration::from_secs(4), stream.read_exact(&mut length))
            .await
            .is_err()
        {
            return;
        }
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 {
            continue;
        }
        if length > 64 * 1024 {
            return;
        }
        let mut message = vec![0; length];
        if tokio::time::timeout(Duration::from_secs(4), stream.read_exact(&mut message))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[tokio::test]
async fn seeding_does_not_redial_a_tracker_reannounced_confirmed_seeder() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    std::fs::write(output_dir.path().join("seed-peer-reuse.bin"), b"abc")
        .expect("preseed the verified torrent payload");

    let placeholder = one_piece_torrent("http://127.0.0.1:9/announce");
    let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("torrent metadata parses");
    let peer = FullSeeder::start(metadata.info_hash.bytes).await;
    let tracker =
        MockTrackerServer::start_with_event_peers(Vec::new(), vec![peer.addr.port()], 1).await;
    let torrent = one_piece_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--check-integrity=true".to_owned(),
        "--bt-hash-check-seed=true".to_owned(),
        "--seed-time=0.2".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let mut client = RunningAria2::start_rpc(&args);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([
            base64::engine::general_purpose::STANDARD.encode(torrent),
            [],
            {}
        ]),
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
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["completedLength"] == "3" {
            assert_eq!(status["status"], "active");
            break;
        }
        assert!(
            Instant::now() < completion_deadline,
            "preseeded torrent did not enter active seeding: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tracker.wait_for_event("completed").await;
    assert!(
        tracker
            .wait_for_query_count(4, Duration::from_secs(8))
            .await,
        "the tracker should repeatedly rediscover the same complete peer"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        while peer.handshake_count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the seeding coordinator should connect to the discovered seed once");
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        peer.handshake_count(),
        1,
        "a confirmed seed must not be redialed after subsequent tracker responses"
    );

    let _ = rpc(&client, 3, "aria2.forceShutdown", json!([]));
    let exit = client.wait_for_exit(Duration::from_secs(10));
    assert!(exit.success(), "aria2c exits cleanly: {exit}");
}
