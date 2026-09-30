#![cfg(feature = "bittorrent")]

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
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use support::RunningAria2;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve an ephemeral BT listener port")
        .local_addr()
        .expect("bound listener has an address")
        .port()
}

fn torrent(tracker_url: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(32));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"disconnect.bin".to_vec()),
    );
    info.insert(b"piece length".to_vec(), BencodeValue::Int(16));
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(vec![
            0x19, 0xb1, 0x92, 0x8d, 0x58, 0xa2, 0x03, 0x0d, 0x08, 0x02, 0x3f, 0x3d, 0x70, 0x54,
            0x51, 0x6d, 0xbc, 0x18, 0x6f, 0x20, 0xeb, 0xa6, 0x29, 0x20, 0x22, 0xb9, 0xd8, 0xaf,
            0xd8, 0x9b, 0x10, 0x1c, 0x23, 0x55, 0xe1, 0x79, 0x07, 0x93, 0xfb, 0x3b,
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

fn rpc(client: &RunningAria2, id: u64, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params});
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

struct PieceSource {
    addr: SocketAddr,
    accepted_connections: Arc<AtomicUsize>,
    valid_handshakes: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    observed_handshake: Arc<Mutex<Vec<u8>>>,
    task: JoinHandle<()>,
}

impl PieceSource {
    async fn start(info_hash: [u8; 20]) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local source peer");
        let addr = listener
            .local_addr()
            .expect("source listener has an address");
        let accepted_connections = Arc::new(AtomicUsize::new(0));
        let valid_handshakes = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let observed_handshake = Arc::new(Mutex::new(Vec::new()));
        let accepted_connections_task = Arc::clone(&accepted_connections);
        let valid_handshakes_task = Arc::clone(&valid_handshakes);
        let requests_task = Arc::clone(&requests);
        let observed_handshake_task = Arc::clone(&observed_handshake);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                accepted_connections_task.fetch_add(1, Ordering::SeqCst);
                let mut handshake = [0u8; 68];
                let read_result =
                    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut handshake))
                        .await;
                *observed_handshake_task
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = handshake.to_vec();
                if !matches!(read_result, Ok(Ok(_)))
                    || handshake[0] != 19
                    || handshake[28..48] != info_hash
                {
                    continue;
                }
                valid_handshakes_task.fetch_add(1, Ordering::SeqCst);
                let mut response = [0u8; 68];
                response[0] = 19;
                response[1..20].copy_from_slice(b"BitTorrent protocol");
                response[28..48].copy_from_slice(&info_hash);
                response[48..68].fill(0x53);
                if stream.write_all(&response).await.is_err()
                    || stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await.is_err()
                    || stream.write_all(&[0, 0, 0, 1, 1]).await.is_err()
                {
                    continue;
                }

                loop {
                    let Ok(message) = read_message(&mut stream).await else {
                        break;
                    };
                    if message.len() != 13 || message[0] != 6 {
                        continue;
                    }
                    requests_task.fetch_add(1, Ordering::SeqCst);
                    let piece_index = u32::from_be_bytes(message[1..5].try_into().unwrap());
                    let offset = u32::from_be_bytes(message[5..9].try_into().unwrap());
                    let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
                    if piece_index != 0 || offset != 0 || length != 16 {
                        continue;
                    }
                    let mut piece = Vec::with_capacity(25);
                    piece.extend_from_slice(&25u32.to_be_bytes());
                    piece.push(7);
                    piece.extend_from_slice(&0u32.to_be_bytes());
                    piece.extend_from_slice(&0u32.to_be_bytes());
                    piece.extend_from_slice(&[0x41; 16]);
                    if stream.write_all(&piece).await.is_err() {
                        break;
                    }
                    return;
                }
            }
        });
        Self {
            addr,
            accepted_connections,
            valid_handshakes,
            requests,
            observed_handshake,
            task,
        }
    }
}

impl Drop for PieceSource {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_message<R: AsyncRead + Unpin>(stream: &mut R) -> std::io::Result<Vec<u8>> {
    let length = stream.read_u32().await?;
    let mut message = vec![0; length as usize];
    stream.read_exact(&mut message).await?;
    Ok(message)
}

struct PeerWireEvent {
    peer_index: usize,
    message: Vec<u8>,
    unchoke_generation: usize,
}

struct ConnectedLeecher {
    writer: Option<OwnedWriteHalf>,
    am_choking: watch::Receiver<bool>,
    unchoke_generation: Arc<AtomicUsize>,
    reader_task: Option<JoinHandle<()>>,
}

impl ConnectedLeecher {
    fn am_choking(&self) -> bool {
        *self.am_choking.borrow()
    }

    fn unchoke_generation(&self) -> usize {
        self.unchoke_generation.load(Ordering::SeqCst)
    }

    async fn wait_for_remote_disconnect(&mut self) {
        let reader_task = self
            .reader_task
            .take()
            .expect("peer reader task should still be running");
        tokio::time::timeout(Duration::from_secs(2), reader_task)
            .await
            .expect("peer actor did not close the disconnected TCP connection")
            .expect("peer wire reader task panicked");
    }
}

impl Drop for ConnectedLeecher {
    fn drop(&mut self) {
        if let Some(reader_task) = self.reader_task.take() {
            reader_task.abort();
        }
    }
}

async fn connect_interested_leecher(
    listen_port: u16,
    info_hash: [u8; 20],
    peer_id_byte: u8,
    peer_index: usize,
    wire_events: mpsc::UnboundedSender<PeerWireEvent>,
) -> ConnectedLeecher {
    let mut stream = TcpStream::connect(("127.0.0.1", listen_port))
        .await
        .expect("connect an incoming leecher");
    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&info_hash);
    handshake[48..68].fill(peer_id_byte);
    stream
        .write_all(&handshake)
        .await
        .expect("send incoming handshake");
    let mut response = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut response))
        .await
        .expect("incoming handshake response timed out")
        .expect("read incoming handshake response");
    assert_eq!(&response[28..48], &info_hash);
    stream
        .write_all(&[0, 0, 0, 1, 2])
        .await
        .expect("send Interested");
    let (mut reader, writer) = stream.into_split();
    let (am_choking_tx, am_choking) = watch::channel(true);
    let unchoke_generation = Arc::new(AtomicUsize::new(0));
    let reader_generation = Arc::clone(&unchoke_generation);
    let reader_task = tokio::spawn(async move {
        loop {
            let message = match read_message(&mut reader).await {
                Ok(message) => message,
                Err(_) => break,
            };
            match message.first() {
                Some(0) => am_choking_tx.send_replace(true),
                Some(1) => am_choking_tx.send_replace(false),
                _ => *am_choking_tx.borrow(),
            };
            let unchoke_generation = if message.first() == Some(&1) {
                reader_generation.fetch_add(1, Ordering::SeqCst) + 1
            } else {
                reader_generation.load(Ordering::SeqCst)
            };
            if wire_events
                .send(PeerWireEvent {
                    peer_index,
                    message,
                    unchoke_generation,
                })
                .is_err()
            {
                break;
            }
        }
    });
    ConnectedLeecher {
        writer: Some(writer),
        am_choking,
        unchoke_generation,
        reader_task: Some(reader_task),
    }
}

#[tokio::test]
async fn cli_logs_tracker_source_for_discovered_peer_connection() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let log_path = output_dir.path().join("aria2.log");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = torrent(&placeholder_tracker.announce_url());
    let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent parses");
    let source = PieceSource::start(metadata.info_hash.bytes).await;
    let initial_dead_port = reserve_loopback_port();
    let later_dead_port = reserve_loopback_port();
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start_with_event_peers(
        vec![initial_dead_port],
        vec![later_dead_port, source.addr.port()],
        1,
    )
    .await;
    let torrent = torrent(&tracker.announce_url());
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--log={}", log_path.display()),
        "--log-level=debug".to_owned(),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--bt-tracker-interval=1".to_owned(),
        "--seed-time=0".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let encoded = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(&client, 1, "aria2.addTorrent", json!([encoded, [], {}]))
        .as_str()
        .expect("addTorrent returns GID")
        .to_owned();

    let downloaded_first_piece = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["completedLength"] == "16" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    if downloaded_first_piece.is_err() {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        panic!(
            "the peer returned by the later tracker announce did not complete the first piece; tracker_queries={:?}; requests={}; log={log}",
            tracker.captured_queries().await,
            source.requests.load(Ordering::SeqCst),
        );
    }

    let log = std::fs::read_to_string(&log_path).expect("aria2 should write its debug log");
    let tracker_dial_succeeded = log.lines().any(|line| {
        line.contains("Connected to discovered peer") && line.contains("source=Tracker")
    });
    assert!(
        tracker_dial_succeeded,
        "successful tracker-discovered dial must be logged with its source, not mislabeled as PEX: {log}"
    );
    assert!(
        !log.lines().any(|line| {
            (line.contains("Connected to discovered peer")
                || line.contains("Failed to connect discovered peer"))
                && line.contains("[PEX]")
        }),
        "generic peer dial results must not be labeled as PEX: {log}"
    );
}

#[tokio::test]
async fn cli_rebalances_unchoke_when_interested_peer_disconnects_during_idle_wait() {
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let log_path = output_dir.path().join("aria2.log");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = torrent(&placeholder_tracker.announce_url());
    let metadata = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&placeholder)
        .expect("test torrent parses");
    let source = PieceSource::start(metadata.info_hash.bytes).await;
    drop(placeholder_tracker);
    let tracker = MockTrackerServer::start(source.addr.port()).await;
    let torrent = torrent(&tracker.announce_url());
    let listen_port = reserve_loopback_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--log={}", log_path.display()),
        "--log-level=debug".to_owned(),
        format!("--listen-port={listen_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--seed-time=0".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let encoded = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(&client, 1, "aria2.addTorrent", json!([encoded, [], {}]))
        .as_str()
        .expect("addTorrent returns GID")
        .to_owned();

    let first_piece_result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let status = rpc(
                &client,
                2,
                "aria2.tellStatus",
                json!([gid, ["status", "completedLength", "totalLength"]]),
            );
            if status["status"] == "active" && status["completedLength"] == "16" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    if first_piece_result.is_err() {
        let status = rpc(
            &client,
            20,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "errorMessage"]
            ]),
        );
        let peers = rpc(&client, 21, "aria2.getPeers", json!([gid]));
        panic!(
            "first piece stalled: status={status}; peers={peers}; tracker_queries={:?}; source_addr={}; source_connections={}; valid_handshakes={}; requests={}; source_handshake={:02x?}; expected_info_hash={:02x?}",
            tracker.captured_queries().await,
            source.addr,
            source.accepted_connections.load(Ordering::SeqCst),
            source.valid_handshakes.load(Ordering::SeqCst),
            source.requests.load(Ordering::SeqCst),
            source
                .observed_handshake
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            metadata.info_hash.bytes,
        );
    }

    let source_peer_id = char::from(0x53).to_string().repeat(20);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let peers = rpc(&client, 22, "aria2.getPeers", json!([gid]));
            if peers
                .as_array()
                .is_some_and(|peers| peers.iter().all(|peer| peer["peerId"] != source_peer_id))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the piece source should disconnect and leave the idle swarm before slot setup");

    let peer_ids = (0x61..=0x65)
        .map(|peer_id_byte| char::from(peer_id_byte).to_string().repeat(20))
        .collect::<Vec<_>>();
    let (wire_events_tx, mut wire_events_rx) = mpsc::unbounded_channel();
    let mut peers = Vec::with_capacity(peer_ids.len());
    for (peer_index, peer_id_byte) in (0x61..=0x65).enumerate() {
        peers.push(
            connect_interested_leecher(
                listen_port,
                metadata.info_hash.bytes,
                peer_id_byte,
                peer_index,
                wire_events_tx.clone(),
            )
            .await,
        );
    }
    drop(wire_events_tx);

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let details = rpc(&client, 4, "aria2.getPeerDetails", json!([gid]));
            let snapshots = rpc(&client, 3, "aria2.getPeers", json!([gid]));
            let snapshots = snapshots.as_array().expect("getPeers returns an array");
            let wire_am_choking = peers
                .iter()
                .map(ConnectedLeecher::am_choking)
                .collect::<Vec<_>>();
            let all_present = peer_ids
                .iter()
                .all(|peer_id| snapshots.iter().any(|peer| peer["peerId"] == *peer_id));
            let all_interested = peer_ids.iter().all(|peer_id| {
                details.as_array().is_some_and(|details| {
                    details.iter().any(|peer| {
                        peer["peerId"] == *peer_id && peer["flags"]["peerInterested"] == true
                    })
                })
            });
            let rpc_matches_wire = peer_ids.iter().enumerate().all(|(index, peer_id)| {
                snapshots.iter().any(|peer| {
                    peer["peerId"] == *peer_id
                        && peer["amChoking"]
                            == if wire_am_choking[index] { "true" } else { "false" }
                })
            });
            let has_choked_and_unchoked = wire_am_choking.iter().any(|&choking| choking)
                && wire_am_choking.iter().any(|&choking| !choking);
            if all_present && all_interested && rpc_matches_wire && has_choked_and_unchoked {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let wire_am_choking = peers
            .iter()
            .map(ConnectedLeecher::am_choking)
            .collect::<Vec<_>>();
        let snapshots = rpc(&client, 30, "aria2.getPeers", json!([gid]));
        panic!(
            "five connected interested peers did not settle into matching wire/RPC upload slots: peers={snapshots}; wire_am_choking={wire_am_choking:?}; expected_peer_ids={peer_ids:?}"
        );
    });

    let wire_am_choking = peers
        .iter()
        .map(ConnectedLeecher::am_choking)
        .collect::<Vec<_>>();
    let disconnected_index = wire_am_choking
        .iter()
        .position(|&am_choking| !am_choking)
        .expect("one peer owns an upload slot");
    let disconnected_peer_id = peer_ids[disconnected_index].as_str();
    let initially_choked = wire_am_choking.clone();
    let unchoke_generations = peers
        .iter()
        .map(ConnectedLeecher::unchoke_generation)
        .collect::<Vec<_>>();
    drop(peers[disconnected_index].writer.take());
    peers[disconnected_index].wait_for_remote_disconnect().await;

    let promoted_index = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let event = wire_events_rx
                .recv()
                .await
                .expect("peer wire event stream remains open");
            if event.message.first() == Some(&1)
                && initially_choked[event.peer_index]
                && event.unchoke_generation > unchoke_generations[event.peer_index]
            {
                break event.peer_index;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        let snapshots = rpc(&client, 33, "aria2.getPeers", json!([gid]));
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let relevant_log = log
            .lines()
            .filter(|line| {
                line.contains("BT idle peer event")
                    || line.contains("BT no-piece peer event")
                    || line.contains("Removing dead BT peer")
                    || line.contains("Choking algorithm")
            })
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "no previously choked candidate received a post-disconnect Unchoke: peers={snapshots}; choker_log={relevant_log}"
        );
    });
    assert_ne!(promoted_index, disconnected_index);
    // The actor writes Unchoke before publishing ChokeStateChanged to the
    // swarm-owned RPC snapshot. Observe eventual snapshot convergence instead
    // of assuming the read model is updated before the remote socket sees bytes.
    let _snapshots = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshots = rpc(&client, 31, "aria2.getPeers", json!([gid]));
            let converged = snapshots.as_array().is_some_and(|snapshots| {
                snapshots
                    .iter()
                    .all(|peer| peer["peerId"] != disconnected_peer_id)
                    && snapshots.iter().any(|peer| {
                        peer["peerId"] == peer_ids[promoted_index]
                            && peer["amChoking"] == "false"
                    })
            });
            if converged {
                break snapshots;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let snapshots = rpc(&client, 34, "aria2.getPeers", json!([gid]));
        panic!(
            "RPC state did not converge to the wire-observed disconnect and promoted peer: {snapshots}"
        );
    });

    let status = rpc(
        &client,
        4,
        "aria2.tellStatus",
        json!([gid, ["status", "completedLength", "totalLength"]]),
    );
    assert_eq!(status["status"], "active");
    assert_eq!(status["completedLength"], "16");
}
