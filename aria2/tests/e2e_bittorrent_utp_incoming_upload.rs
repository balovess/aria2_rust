#![cfg(feature = "bittorrent")]

//! Exercises an inbound uTP peer through the real CLI process and RPC view.

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;
#[path = "support/mod.rs"]
mod support;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use aria2_protocol::bittorrent::utp::{ConnectionState, UtpSocket};
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::time::{Duration, Instant};
use support::RunningAria2;

const SEED: &[u8] = b"seed";
const SEED_SHA1: [u8; 20] = [
    0x92, 0x71, 0x3d, 0x47, 0x09, 0x37, 0x71, 0x11, 0xcf, 0x31, 0xf2, 0xa7, 0x19, 0x86, 0xc4, 0x11,
    0xbd, 0x6c, 0xb5, 0xb0,
];

fn single_piece_torrent(announce_url: &str) -> Vec<u8> {
    let info = BTreeMap::from([
        (b"name".to_vec(), BencodeValue::Bytes(b"seed.bin".to_vec())),
        (b"length".to_vec(), BencodeValue::Int(SEED.len() as i64)),
        (
            b"piece length".to_vec(),
            BencodeValue::Int(SEED.len() as i64),
        ),
        (b"pieces".to_vec(), BencodeValue::Bytes(SEED_SHA1.to_vec())),
    ]);
    BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(announce_url.as_bytes().to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]))
    .encode()
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
    assert_eq!(response.status, 200, "RPC HTTP status: {}", response.status);
    let response: Value = serde_json::from_slice(&response.body).expect("RPC JSON response");
    assert!(response.get("error").is_none(), "RPC error: {response}");
    response["result"].clone()
}

fn reserve_udp_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0")
        .expect("reserve uTP listener port")
        .local_addr()
        .expect("read reserved uTP listener port")
        .port()
}

async fn wait_for_seed(client: &RunningAria2, gid: &str) {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = rpc(
            client,
            2,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "totalLength"]]),
        );
        if status["status"] == "active" && status["completedLength"] == SEED.len().to_string() {
            assert_eq!(status["totalLength"], SEED.len().to_string());
            return;
        }
        assert!(
            Instant::now() < deadline,
            "torrent did not enter seeding: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn next_utp_payload(socket: &mut UtpSocket, conn_id: u16) -> Vec<u8> {
    loop {
        let payloads = socket.poll_recv().expect("poll uTP peer socket");
        if let Some((_, payload)) = payloads.into_iter().find(|(id, _)| *id == conn_id) {
            return payload;
        }

        let readiness = socket
            .readiness_socket()
            .expect("get uTP peer socket readiness");
        if let Some(delay) = socket.next_timer_delay() {
            tokio::select! {
                ready = readiness.readable() => ready.expect("wait for uTP peer traffic"),
                _ = tokio::time::sleep(delay) => socket.process_timers().expect("process uTP peer timers"),
            }
        } else {
            readiness
                .readable()
                .await
                .expect("wait for uTP peer traffic");
        }
    }
}

async fn next_bt_message(socket: &mut UtpSocket, conn_id: u16, buffer: &mut Vec<u8>) -> Vec<u8> {
    loop {
        if buffer.len() >= 4 {
            let message_len =
                u32::from_be_bytes(buffer[..4].try_into().expect("four-byte BT frame length"))
                    as usize;
            if message_len == 0 {
                buffer.drain(..4);
                continue;
            }
            if buffer.len() >= message_len + 4 {
                return buffer.drain(..message_len + 4).collect();
            }
        }
        buffer.extend_from_slice(&next_utp_payload(socket, conn_id).await);
    }
}

fn send_utp_bytes(socket: &mut UtpSocket, conn_id: u16, bytes: &[u8]) {
    let sent = socket.send(conn_id, bytes).expect("send BT data over uTP");
    assert_eq!(sent, bytes.len(), "uTP peer must send the complete frame");
}

async fn connect_and_upload_one_piece(
    mut socket: UtpSocket,
    address: std::net::SocketAddr,
    info_hash: [u8; 20],
    client: &RunningAria2,
    gid: &str,
    log_path: &std::path::Path,
) -> Vec<u8> {
    let peer_id = [0x44; 20];
    let conn_id = socket
        .connect(address)
        .expect("initiate incoming uTP connection");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = socket.poll_recv().expect("complete uTP connection setup");
        match socket.connection_state(conn_id) {
            Ok(ConnectionState::Established) => break,
            Ok(ConnectionState::Closed | ConnectionState::Closing | ConnectionState::TimeWait) => {
                panic!("uTP connection closed before BitTorrent handshake")
            }
            Ok(_) => {}
            Err(error) => panic!("uTP connection disappeared during setup: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "aria2 did not accept an inbound uTP connection"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    send_utp_bytes(
        &mut socket,
        conn_id,
        &Handshake::new(&info_hash, &peer_id).to_bytes(),
    );
    let mut bt_buffer = Vec::new();
    let handshake_deadline = Instant::now() + Duration::from_secs(5);
    while bt_buffer.len() < 68 {
        let remaining = handshake_deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "aria2 accepted the uTP transport but did not route the inbound BitTorrent handshake"
        );
        let payload = tokio::time::timeout(
            remaining,
            next_utp_payload(&mut socket, conn_id),
        )
        .await
        .expect("aria2 accepted the uTP transport but did not route the inbound BitTorrent handshake");
        bt_buffer.extend_from_slice(&payload);
    }
    let handshake = Handshake::parse(&bt_buffer[..68]).expect("parse aria2 BT handshake");
    assert_eq!(handshake.info_hash, info_hash);
    assert_ne!(handshake.peer_id, peer_id);
    bt_buffer.drain(..68);

    // Interested followed by a request for the only verified piece.
    send_utp_bytes(&mut socket, conn_id, &[0, 0, 0, 1, 2]);
    let mut unchoked = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    while !unchoked {
        if Instant::now() >= deadline {
            let peers = rpc(client, 44, "aria2.getPeers", json!([gid]));
            let status = rpc(
                client,
                45,
                "aria2.tellStatus",
                json!([
                    gid,
                    [
                        "status",
                        "connections",
                        "numSeeders",
                        "uploadLength",
                        "errorMessage"
                    ]
                ]),
            );
            let log = std::fs::read_to_string(log_path).unwrap_or_default();
            let log_tail = log
                .lines()
                .rev()
                .take(80)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            panic!(
                "aria2 did not unchoke the interested peer; peers={peers}; status={status}; log tail:\n{log_tail}"
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Ok(message) = tokio::time::timeout(
            remaining,
            next_bt_message(&mut socket, conn_id, &mut bt_buffer),
        )
        .await
        {
            unchoked = message.get(4) == Some(&1);
        }
    }

    let mut request = Vec::with_capacity(17);
    request.extend_from_slice(&13u32.to_be_bytes());
    request.push(6);
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&(SEED.len() as u32).to_be_bytes());
    send_utp_bytes(&mut socket, conn_id, &request);

    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        assert!(
            Instant::now() < deadline,
            "aria2 did not upload the requested uTP piece"
        );
        let message = tokio::time::timeout(
            Duration::from_secs(2),
            next_bt_message(&mut socket, conn_id, &mut bt_buffer),
        )
        .await
        .expect("wait for requested piece over uTP");
        if message.get(4) == Some(&7) {
            assert_eq!(&message[5..9], &0u32.to_be_bytes());
            assert_eq!(&message[9..13], &0u32.to_be_bytes());
            let expected_peer_id = String::from_utf8_lossy(&peer_id).into_owned();
            let peer_details_deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let details = rpc(client, 46, "aria2.getPeerDetails", json!([gid]));
                if details.as_array().is_some_and(|peers| {
                    peers.iter().any(|peer| {
                        peer["peerId"] == expected_peer_id
                            && peer["source"] == "incoming"
                            && peer["flags"]["peerInterested"] == true
                            && peer["flags"]["amChoking"] == false
                    })
                }) {
                    break;
                }
                assert!(
                    Instant::now() < peer_details_deadline,
                    "RPC did not expose the live incoming uTP uploader and unchoked state: {details}"
                );
                tokio::task::yield_now().await;
            }
            return message[13..].to_vec();
        }
    }
}

#[tokio::test]
async fn cli_rpc_seeder_accepts_inbound_utp_peer_and_uploads_verified_piece() {
    let temp = tempfile::tempdir().expect("temporary seed directory");
    std::fs::write(temp.path().join("seed.bin"), SEED).expect("write seeded payload");
    let tracker = MockTrackerServer::start_with_peers(Vec::new(), false).await;
    let torrent = single_piece_torrent(&tracker.announce_url());
    let info_hash = TorrentMeta::parse(&torrent)
        .expect("single-piece torrent should parse")
        .info_hash
        .bytes;
    let utp_port = reserve_udp_port();
    let log_path = temp.path().join("aria2.log");
    let args = [
        format!("--dir={}", temp.path().display()),
        format!("--log={}", log_path.display()),
        "--log-level=debug".to_owned(),
        "--enable-utp=true".to_owned(),
        format!("--utp-listen-port={utp_port}"),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
        "--enable-lpd=false".to_owned(),
        "--bt-enable-web-seed=false".to_owned(),
        "--check-integrity=true".to_owned(),
        "--bt-hash-check-seed=true".to_owned(),
        "--seed-time=30".to_owned(),
        "--seed-ratio=0".to_owned(),
    ];
    let client = RunningAria2::start_rpc(&args);
    let gid = rpc(
        &client,
        1,
        "aria2.addTorrent",
        json!([
            base64::engine::general_purpose::STANDARD.encode(&torrent),
            [],
            {}
        ]),
    )
    .as_str()
    .expect("addTorrent returns a GID")
    .to_owned();
    wait_for_seed(&client, &gid).await;

    let peer = UdpSocket::bind("127.0.0.1:0").expect("reserve remote TCP test port");
    let peer_port = peer.local_addr().expect("read peer source port").port();
    drop(peer);
    let peer_socket =
        UtpSocket::bind(&format!("127.0.0.1:{peer_port}")).expect("bind controlled uTP peer");
    let payload = tokio::time::timeout(
        Duration::from_secs(25),
        connect_and_upload_one_piece(
            peer_socket,
            std::net::SocketAddr::from(([127, 0, 0, 1], utp_port)),
            info_hash,
            &client,
            &gid,
            &log_path,
        ),
    )
    .await
    .expect("inbound uTP peer/upload E2E timed out");
    assert_eq!(
        payload, SEED,
        "uTP upload must match the verified torrent piece"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rpc(
            &client,
            4,
            "aria2.tellStatus",
            json!([gid, ["status", "completedLength", "uploadLength"]]),
        );
        if status["uploadLength"] == SEED.len().to_string() {
            assert_eq!(status["status"], "active");
            assert_eq!(status["completedLength"], SEED.len().to_string());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "RPC upload stats did not converge: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
