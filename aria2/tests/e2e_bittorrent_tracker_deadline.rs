#![cfg(feature = "bittorrent")]

//! Process-level tracker scheduling coverage while a peer request is pending.

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
use std::time::Duration;
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};

const PAYLOAD: &[u8] = b"abc";
const SLOW_BLOCK_DELAY: Duration = Duration::from_secs(4);

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
        .expect("reserve a loopback port")
        .local_addr()
        .expect("bound socket has a local address")
        .port()
}

fn torrent_bytes(tracker_url: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(PAYLOAD.len() as i64));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"tracker-deadline.bin".to_vec()),
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

fn torrent_bytes_with_tiers(announce_url: &str, tiers: &[Vec<String>]) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(PAYLOAD.len() as i64));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"udp-tracker-failover.bin".to_vec()),
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
        BencodeValue::Bytes(announce_url.as_bytes().to_vec()),
    );
    root.insert(
        b"announce-list".to_vec(),
        BencodeValue::List(
            tiers
                .iter()
                .map(|tier| {
                    BencodeValue::List(
                        tier.iter()
                            .map(|url| BencodeValue::Bytes(url.as_bytes().to_vec()))
                            .collect(),
                    )
                })
                .collect(),
        ),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

struct SilentUdpTracker {
    addr: SocketAddr,
    connect_count: watch::Receiver<usize>,
    task: JoinHandle<()>,
}

impl SilentUdpTracker {
    async fn start() -> Self {
        let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind silent UDP tracker");
        let addr = socket.local_addr().expect("read UDP tracker address");
        let (connect_count_tx, connect_count) = watch::channel(0usize);
        let task = tokio::spawn(async move {
            let mut packet = [0u8; 1500];
            while let Ok((length, _)) = socket.recv_from(&mut packet).await {
                if length >= 12 && i32::from_be_bytes(packet[8..12].try_into().unwrap()) == 0 {
                    connect_count_tx.send_modify(|count| *count += 1);
                }
                // Deliberately remain silent: retries and tracker failover are
                // the behavior under test.
            }
        });
        Self {
            addr,
            connect_count,
            task,
        }
    }

    async fn wait_for_connects(&mut self, expected: usize, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            while *self.connect_count.borrow_and_update() < expected {
                if self.connect_count.changed().await.is_err() {
                    return false;
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    }
}

impl Drop for SilentUdpTracker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct SlowSeeder {
    addr: SocketAddr,
    request_seen: watch::Receiver<bool>,
    task: JoinHandle<()>,
}

impl SlowSeeder {
    async fn start(info_hash: [u8; 20]) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slow seeder");
        let addr = listener.local_addr().expect("seeder local address");
        let (request_seen_tx, request_seen) = watch::channel(false);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        connections.spawn(serve_peer(
                            stream,
                            info_hash,
                            request_seen_tx.clone(),
                        ));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            request_seen,
            task,
        }
    }

    async fn wait_for_request(&mut self) {
        let seen = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if *self.request_seen.borrow_and_update() {
                    return true;
                }
                if self.request_seen.changed().await.is_err() {
                    return false;
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(seen, "aria2 never requested the test piece");
    }
}

impl Drop for SlowSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_peer(mut stream: TcpStream, info_hash: [u8; 20], request_seen: watch::Sender<bool>) {
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
    response[27] |= 0x04;
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].copy_from_slice(b"SlowSeeder-000000000");
    if stream.write_all(&response).await.is_err()
        || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
        || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
    {
        return;
    }

    loop {
        let Ok(length) = stream.read_u32().await else {
            return;
        };
        if length == 0 {
            continue;
        }
        if length > 64 * 1024 {
            return;
        }
        let mut message = vec![0u8; length as usize];
        if stream.read_exact(&mut message).await.is_err() {
            return;
        }
        if message.len() != 13 || message.first() != Some(&6) {
            continue;
        }

        let piece_index = u32::from_be_bytes(message[1..5].try_into().unwrap());
        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
        let block_length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
        let Some(end) = begin.checked_add(block_length) else {
            return;
        };
        if piece_index != 0 || end > PAYLOAD.len() {
            continue;
        }

        request_seen.send_replace(true);
        tokio::time::sleep(SLOW_BLOCK_DELAY).await;
        let block = &PAYLOAD[begin..end];
        let mut piece = Vec::with_capacity(4 + 9 + block.len());
        piece.extend_from_slice(&((9 + block.len()) as u32).to_be_bytes());
        piece.push(7);
        piece.extend_from_slice(&piece_index.to_be_bytes());
        piece.extend_from_slice(&(begin as u32).to_be_bytes());
        piece.extend_from_slice(block);
        let _ = stream.write_all(&piece).await;
        return;
    }
}

#[tokio::test]
async fn cli_announces_tracker_deadline_while_piece_response_is_pending() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder = torrent_bytes("http://127.0.0.1:9/announce");
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let mut peer = SlowSeeder::start(meta.info_hash.bytes).await;
    let tracker =
        MockTrackerServer::start_with_peers_and_interval(vec![peer.addr.port()], false, 1).await;
    let torrent = torrent_bytes(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-request-timeout=10".to_owned(),
        // aria2 expresses seed-time in fractional minutes. Keep the seed
        // lifecycle short while still exercising its terminal tracker event.
        "--seed-time=0.02".to_owned(),
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

    assert!(
        tracker
            .wait_for_query_count(1, Duration::from_secs(5))
            .await,
        "the initial tracker announce did not arrive"
    );
    peer.wait_for_request().await;

    assert!(
        tracker
            .wait_for_query_count(2, Duration::from_secs(2))
            .await,
        "the due tracker announce was delayed until the active piece batch returned; queries: {:?}",
        tracker.captured_queries().await
    );
    let status = rpc(
        &client,
        2,
        "aria2.tellStatus",
        json!([gid, ["status", "completedLength", "totalLength"]]),
    );
    assert_eq!(
        status["completedLength"], "0",
        "piece should still be pending"
    );

    tracker.wait_for_event("completed").await;
    assert_eq!(
        std::fs::read(output_dir.path().join("tracker-deadline.bin"))
            .expect("completed payload is readable"),
        PAYLOAD
    );
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn cli_announces_stopped_after_completion_when_seeding_is_disabled() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder = torrent_bytes("http://127.0.0.1:9/announce");
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let mut peer = SlowSeeder::start(meta.info_hash.bytes).await;
    let tracker =
        MockTrackerServer::start_with_peers_and_interval(vec![peer.addr.port()], false, 1).await;
    let torrent = torrent_bytes(&tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=0".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let torrent_base64 = base64::engine::general_purpose::STANDARD.encode(torrent);
    let _gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([torrent_base64, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    tracker.wait_for_event("started").await;
    peer.wait_for_request().await;
    tracker.wait_for_event("completed").await;
    assert_eq!(
        std::fs::read(output_dir.path().join("tracker-deadline.bin"))
            .expect("completed payload is readable"),
        PAYLOAD
    );
    tracker.wait_for_event("stopped").await;
}

#[tokio::test]
async fn cli_uses_tracker_tier_returned_by_http_announce_and_reports_it_over_rpc() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let placeholder = torrent_bytes("http://127.0.0.1:9/announce");
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let mut peer = SlowSeeder::start(meta.info_hash.bytes).await;
    let discovered_tracker =
        MockTrackerServer::start_with_peers_and_interval(Vec::new(), false, 1).await;
    let initial_tracker = MockTrackerServer::start_with_dynamic_announce_list(
        vec![peer.addr.port()],
        1,
        vec![vec![discovered_tracker.announce_url()]],
        Some(2),
    )
    .await;
    let torrent = torrent_bytes(&initial_tracker.announce_url());
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-request-timeout=10".to_owned(),
        "--seed-time=0".to_owned(),
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

    assert!(
        initial_tracker
            .wait_for_query_count(1, Duration::from_secs(5))
            .await,
        "the initial tracker announce did not arrive"
    );
    peer.wait_for_request().await;
    assert!(
        initial_tracker
            .wait_for_query_count(2, Duration::from_secs(4))
            .await,
        "the initial tracker did not receive the expected failover-triggering announce"
    );
    assert!(
        discovered_tracker
            .wait_for_query_count(1, Duration::from_secs(4))
            .await,
        "the dynamically returned tracker tier was not used after failover"
    );

    let trackers = rpc(&client, 2, "aria2.getTrackers", json!([gid]));
    let dynamic_tracker = trackers
        .as_array()
        .expect("getTrackers returns a list")
        .iter()
        .find(|tracker| tracker["uri"] == discovered_tracker.announce_url())
        .expect("the dynamically returned tracker is visible through RPC");
    assert_eq!(dynamic_tracker["tier"], 2);
    assert_eq!(dynamic_tracker["status"], "succeeded");
    let status = rpc(
        &client,
        3,
        "aria2.tellStatus",
        json!([gid, ["completedLength", "totalLength"]]),
    );
    assert_eq!(
        status["completedLength"], "0",
        "tracker failover should be verified while the peer's piece response is still pending"
    );
}

#[tokio::test]
async fn cli_fails_over_from_silent_udp_tracker_to_http_tracker_and_reports_rpc_state() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let mut udp_tracker = SilentUdpTracker::start().await;
    let udp_url = format!("udp://{}/announce", udp_tracker.addr);
    let placeholder = torrent_bytes_with_tiers(&udp_url, &[vec![udp_url.clone()]]);
    let meta = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent metadata parses");
    let mut peer = SlowSeeder::start(meta.info_hash.bytes).await;
    let http_tracker =
        MockTrackerServer::start_with_peers_and_interval(vec![peer.addr.port()], false, 300).await;
    let torrent = torrent_bytes_with_tiers(
        &udp_url,
        &[vec![udp_url.clone()], vec![http_tracker.announce_url()]],
    );
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-tracker-timeout=60".to_owned(),
        "--bt-request-timeout=10".to_owned(),
        "--seed-time=0".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let torrent_base64 = base64::engine::general_purpose::STANDARD.encode(torrent);
    let started = tokio::time::Instant::now();
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([torrent_base64, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    assert!(
        udp_tracker
            .wait_for_connects(2, Duration::from_secs(9))
            .await,
        "the silent UDP tracker should receive the original CONNECT and its one retry"
    );
    assert!(
        http_tracker
            .wait_for_query_count(1, Duration::from_secs(17))
            .await,
        "the failed UDP tier should advance to the HTTP tracker instead of waiting for the 60-second cap"
    );
    assert!(
        started.elapsed() < Duration::from_secs(24),
        "fallback exceeded the original UDP retry schedule: {:?}",
        started.elapsed()
    );

    let trackers = rpc(&client, 2, "aria2.getTrackers", json!([gid]));
    let trackers = trackers.as_array().expect("getTrackers returns a list");
    let udp_state = trackers
        .iter()
        .find(|tracker| tracker["uri"] == udp_url)
        .expect("silent UDP tracker remains visible in RPC");
    assert_eq!(udp_state["status"], "failed");
    assert_eq!(udp_state["lastFailureKind"], "timeout");
    let http_state = trackers
        .iter()
        .find(|tracker| tracker["uri"] == http_tracker.announce_url())
        .expect("fallback HTTP tracker remains visible in RPC");
    assert_eq!(http_state["status"], "succeeded");

    peer.wait_for_request().await;
    http_tracker.wait_for_event("completed").await;
    assert_eq!(
        std::fs::read(output_dir.path().join("udp-tracker-failover.bin"))
            .expect("downloaded payload is readable"),
        PAYLOAD
    );
    http_tracker.wait_for_event("stopped").await;
}
