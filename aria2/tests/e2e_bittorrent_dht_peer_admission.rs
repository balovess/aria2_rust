#![cfg(feature = "bittorrent")]

//! Process-level DHT peer admission while a piece batch is waiting on a peer.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_bt_peer.rs"]
mod mock_bt_peer;
#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::{
    bencode::codec::BencodeValue,
    dht::message::{DhtMessage, DhtMessageBuilder, DhtQueryMethod},
    torrent::parser::TorrentMeta,
};
use base64::Engine as _;
use mock_bt_peer::MockBtPeerServer;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::{collections::BTreeMap, net::SocketAddr, time::Duration};
use support::RunningAria2;
use tokio::{net::UdpSocket, sync::watch};

const PIECE_LENGTH: usize = 32 * 1024;
const PIECE_SHA1: [u8; 20] = [
    0x51, 0x88, 0x43, 0x18, 0x49, 0xb4, 0x61, 0x31, 0x52, 0xfd, 0x7b, 0xdb, 0xa6, 0xa3, 0xff, 0x0a,
    0x4f, 0xd6, 0x42, 0x4b,
];
const DHT_NODE_ID: [u8; 20] = [0x42; 20];

struct LoopbackDht {
    addr: SocketAddr,
    get_peers_count: watch::Receiver<usize>,
    release_get_peers: tokio::sync::mpsc::UnboundedSender<()>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl LoopbackDht {
    async fn start(peer: SocketAddr) -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0")
            .await
            .expect("bind DHT fixture socket");
        let addr = socket.local_addr().expect("DHT fixture local address");
        let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
        let (query_count, get_peers_count) = watch::channel(0usize);
        let (release_get_peers, mut release_get_peers_rx) =
            tokio::sync::mpsc::unbounded_channel::<()>();

        tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            let mut get_peers_released = false;
            loop {
                tokio::select! {
                    received = socket.recv_from(&mut buffer) => {
                        let Ok((length, source)) = received else { break };
                        let Ok(message) = DhtMessage::decode(&buffer[..length]) else { continue };
                        let Some(method) = message.q.as_ref() else { continue };
                        let transaction = message.t.as_slice();
                        let response = match method.0.as_str() {
                            DhtQueryMethod::PING => {
                                DhtMessageBuilder::ping_response(transaction, &DHT_NODE_ID)
                            }
                            DhtQueryMethod::FIND_NODE => {
                                DhtMessageBuilder::find_node_response(transaction, &DHT_NODE_ID, &[])
                            }
                            DhtQueryMethod::GET_PEERS => {
                                let next_count = *query_count.borrow() + 1;
                                query_count.send_replace(next_count);
                                if !get_peers_released {
                                    let _ = release_get_peers_rx.recv().await;
                                    get_peers_released = true;
                                }
                                DhtMessageBuilder::get_peers_response_with_peers(
                                    transaction,
                                    &DHT_NODE_ID,
                                    b"loopback-token",
                                    &[peer],
                                )
                            }
                            DhtQueryMethod::ANNOUNCE_PEER => {
                                DhtMessageBuilder::announce_peer_response(transaction, &DHT_NODE_ID)
                            }
                            _ => continue,
                        };
                        let _ = socket.send_to(&response.encode(), source).await;
                    }
                    _ = &mut shutdown_rx => break,
                }
            }
        });

        Self {
            addr,
            get_peers_count,
            release_get_peers,
            shutdown: Some(shutdown),
        }
    }

    async fn wait_for_get_peers(&mut self, timeout: Duration) -> bool {
        tokio::time::timeout(timeout, async {
            while *self.get_peers_count.borrow_and_update() == 0 {
                if self.get_peers_count.changed().await.is_err() {
                    return false;
                }
            }
            true
        })
        .await
        .unwrap_or(false)
    }

    fn release_get_peers(&self) {
        let _ = self.release_get_peers.send(());
    }
}

impl Drop for LoopbackDht {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

fn torrent_bytes(tracker_url: &str, piece_count: usize) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(
        b"length".to_vec(),
        BencodeValue::Int((PIECE_LENGTH * piece_count) as i64),
    );
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"dht-peer-admission.bin".to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(PIECE_LENGTH as i64),
    );
    info.insert(
        b"pieces".to_vec(),
        BencodeValue::Bytes(PIECE_SHA1.repeat(piece_count)),
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
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
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

fn status(client: &RunningAria2, gid: &str) -> Value {
    rpc(
        client,
        2,
        "aria2.tellStatus",
        json!([
            gid,
            ["status", "completedLength", "totalLength", "downloadSpeed"]
        ]),
    )
}

fn peer_details(client: &RunningAria2, gid: &str) -> Value {
    rpc(client, 3, "aria2.getPeerDetails", json!([gid]))
}

fn status_u64(value: &Value, field: &str) -> u64 {
    value[field]
        .as_str()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| panic!("tellStatus.{field} is not a u64: {value}"))
}

fn reserve_udp_port() -> u16 {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("reserve a DHT UDP port");
    socket
        .local_addr()
        .expect("read reserved DHT UDP port")
        .port()
}

fn reserve_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a BitTorrent TCP port")
        .local_addr()
        .expect("read reserved TCP port")
        .port()
}

async fn query_dht_peer_values(
    probe: &UdpSocket,
    target: SocketAddr,
    info_hash: &[u8; 20],
    transaction: u32,
) -> Vec<Vec<u8>> {
    let query = DhtMessageBuilder::get_peers(transaction, &[0xA5; 20], info_hash);
    probe
        .send_to(&query.encode(), target)
        .await
        .expect("send loopback get_peers query");
    let mut buffer = [0u8; 4096];
    let Ok(Ok((length, _))) =
        tokio::time::timeout(Duration::from_millis(250), probe.recv_from(&mut buffer)).await
    else {
        return Vec::new();
    };
    DhtMessage::decode(&buffer[..length])
        .ok()
        .and_then(|reply| reply.r)
        .and_then(|result| {
            result
                .dict_get(b"values")
                .and_then(BencodeValue::as_list)
                .cloned()
        })
        .unwrap_or_default()
        .iter()
        .filter_map(BencodeValue::as_bytes)
        .map(<[u8]>::to_vec)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dht_peer_wakes_active_batch_and_existing_peer_actor_is_reused() {
    const PIECE_COUNT: usize = 48;
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let payload = vec![0; PIECE_LENGTH];
    let placeholder = torrent_bytes("http://127.0.0.1:1/announce", PIECE_COUNT);
    let metadata = TorrentMeta::parse(&placeholder).expect("torrent metadata parses");
    let initial_peer = MockBtPeerServer::start_staying_choked(
        metadata.info_hash.bytes,
        vec![payload.clone(); PIECE_COUNT],
    )
    .await;
    let dht_peer =
        MockBtPeerServer::start(metadata.info_hash.bytes, vec![payload.clone(); PIECE_COUNT]).await;
    let mut dht = LoopbackDht::start(dht_peer.addr()).await;
    let tracker = MockTrackerServer::start(initial_peer.addr().port()).await;
    let torrent = torrent_bytes(&tracker.announce_url(), PIECE_COUNT);
    let listen_port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("reserve a loopback peer port")
        .local_addr()
        .expect("read reserved peer port")
        .port();
    let dht_file = output_dir.path().join("dht.dat");
    let dht_listen_port = reserve_udp_port();
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={listen_port}"),
        format!("--bt-request-timeout={}", REQUEST_TIMEOUT.as_secs()),
        format!("--dht-entry-point={}", dht.addr),
        format!("--dht-file-path={}", dht_file.display()),
        format!("--dht-listen-port={dht_listen_port}"),
        "--dht-bootstrap-timeout=3".to_owned(),
        "--dht-message-timeout=2".to_owned(),
        "--enable-dht=true".to_owned(),
        "--enable-dht6=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let encoded_torrent = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([encoded_torrent, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();

    assert!(
        initial_peer
            .wait_for_handshakes(1, Duration::from_secs(6))
            .await,
        "the tracker peer should be connected while it remains choked"
    );
    assert!(
        dht.wait_for_get_peers(Duration::from_secs(6)).await,
        "the running task must issue a real DHT get_peers query"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let peers = peer_details(&client, &gid);
            if peers.as_array().is_some_and(|peers| {
                peers.iter().any(|peer| {
                    peer["source"] == "tracker"
                        && peer["flags"]["amInterested"] == true
                        && peer["flags"]["peerChoking"] == true
                })
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("RPC must show the original tracker actor interested in available pieces while choked");
    assert_eq!(
        dht_peer.completed_handshake_count(),
        0,
        "the DHT peer cannot be contacted before the gated get_peers response"
    );
    let tracker_handshakes_before_dht_admission = initial_peer.completed_handshake_count();
    assert_eq!(
        tracker_handshakes_before_dht_admission, 1,
        "the initial tracker peer must have one live handshake before DHT admission"
    );
    dht.release_get_peers();
    assert!(
        dht_peer
            .wait_for_handshakes(1, Duration::from_secs(5))
            .await,
        "DHT-discovered peer should be dialed before the {REQUEST_TIMEOUT:?} block timeout; status={}",
        status(&client, &gid)
    );

    tokio::time::timeout(Duration::from_secs(6), async {
        loop {
            let snapshot = status(&client, &gid);
            if status_u64(&snapshot, "completedLength") >= (PIECE_LENGTH * 4) as u64 {
                assert!(
                    status_u64(&snapshot, "downloadSpeed") > 0,
                    "the DHT peer's accepted payload should be reflected in RPC: {snapshot}"
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("DHT peer should complete multiple pieces while the batch is active");

    assert!(
        !dht_peer.requested_pieces().await.is_empty(),
        "DHT peer must receive a real piece request"
    );
    let peers = peer_details(&client, &gid);
    assert!(
        peers.as_array().is_some_and(|peers| {
            peers.iter().any(|peer| {
                peer["source"] == "tracker"
                    && peer["flags"]["amInterested"] == true
                    && peer["flags"]["peerChoking"] == true
            }) && peers.iter().any(|peer| peer["source"] == "dht")
        }),
        "the original interested-but-choked tracker actor must remain live beside the DHT actor: {peers}"
    );
    assert_eq!(
        initial_peer.completed_handshake_count(),
        tracker_handshakes_before_dht_admission,
        "DHT peer admission must preserve the pre-existing tracker actor without reconnecting"
    );
    assert_eq!(
        dht_peer.completed_handshake_count(),
        1,
        "the admitted DHT peer should establish one long-lived actor connection"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dht_get_peers_advertises_active_torrent_external_ip() {
    const PIECE_COUNT: usize = 48;
    let output_dir = tempfile::tempdir().expect("temporary download directory");
    let torrent = torrent_bytes("http://127.0.0.1:1/announce", PIECE_COUNT);
    let metadata = TorrentMeta::parse(&torrent).expect("torrent metadata parses");
    let tcp_port = reserve_tcp_port();
    let dht_port = reserve_udp_port();
    let external_ip = "198.51.100.37";
    let updated_external_ip = "198.51.100.38";
    let args = [
        format!("--dir={}", output_dir.path().display()),
        format!("--listen-port={tcp_port}"),
        format!("--dht-listen-port={dht_port}"),
        format!(
            "--dht-file-path={}",
            output_dir.path().join("dht.dat").display()
        ),
        "--dht-bootstrap-timeout=1".to_owned(),
        "--enable-dht=true".to_owned(),
        "--enable-dht6=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        format!("--bt-external-ip={external_ip}"),
    ];
    let client = RunningAria2::start_rpc(&args);
    let encoded_torrent = base64::engine::general_purpose::STANDARD.encode(torrent);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([encoded_torrent, [], {}]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();
    let options = rpc(&client, 2, "aria2.getOption", json!([gid]));
    assert_eq!(options["bt-external-ip"], external_ip);

    let expected_peer = {
        let mut compact = external_ip
            .parse::<std::net::Ipv4Addr>()
            .expect("valid fixture IP")
            .octets()
            .to_vec();
        compact.extend_from_slice(&tcp_port.to_be_bytes());
        compact
    };
    let probe = UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind DHT query socket");
    let target = SocketAddr::from(([127, 0, 0, 1], dht_port));
    let mut last_values = Vec::new();
    let mut last_reply = None;
    tokio::time::timeout(Duration::from_secs(6), async {
        let mut transaction = 1u32;
        loop {
            let query = DhtMessageBuilder::get_peers(transaction, &[0xA5; 20], &metadata.info_hash.bytes);
            probe
                .send_to(&query.encode(), target)
                .await
                .expect("send loopback get_peers query");
            transaction = transaction.wrapping_add(1);

            let mut buffer = [0u8; 4096];
            if let Ok(Ok((length, _))) = tokio::time::timeout(
                Duration::from_millis(250),
                probe.recv_from(&mut buffer),
            )
            .await
                && let Ok(reply) = DhtMessage::decode(&buffer[..length])
            {
                last_reply = Some(format!("type={:?}, response={:?}", reply.y, reply.r));
                last_values = reply
                    .r
                    .as_ref()
                    .and_then(|result| result.dict_get(b"values"))
                    .and_then(BencodeValue::as_list)
                    .map(|values| {
                        values
                            .iter()
                            .filter_map(BencodeValue::as_bytes)
                            .map(<[u8]>::to_vec)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                if last_values.contains(&expected_peer) {
                    break;
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "DHT get_peers for active torrent {gid} should advertise configured local peer {external_ip}:{tcp_port}; values={last_values:?}; last reply={last_reply:?}"
        )
    });

    assert_eq!(
        rpc(
            &client,
            3,
            "aria2.changeOption",
            json!([gid, {"bt-external-ip": updated_external_ip}]),
        ),
        "OK"
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if rpc(&client, 4, "aria2.getOption", json!([gid]))["bt-external-ip"]
                == updated_external_ip
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the pending active-task option must apply on its restart");
    let updated_peer = {
        let mut compact = updated_external_ip
            .parse::<std::net::Ipv4Addr>()
            .expect("valid updated fixture IP")
            .octets()
            .to_vec();
        compact.extend_from_slice(&tcp_port.to_be_bytes());
        compact
    };
    let old_peer = expected_peer;
    let mut last_values = Vec::new();
    tokio::time::timeout(Duration::from_secs(4), async {
        let mut transaction = 10_000u32;
        loop {
            last_values = query_dht_peer_values(
                &probe,
                target,
                &metadata.info_hash.bytes,
                transaction,
            )
            .await;
            transaction = transaction.wrapping_add(1);
            if last_values.contains(&updated_peer) && !last_values.contains(&old_peer) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "DHT must advertise the live bt-external-ip {updated_external_ip}:{tcp_port} and stop advertising {external_ip}:{tcp_port}; values={last_values:?}"
        )
    });
}
