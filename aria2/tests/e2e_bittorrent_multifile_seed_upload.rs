#![cfg(feature = "bittorrent")]

//! Verifies that a real seeding peer serves blocks across multi-file boundaries.

#[path = "support/mod.rs"]
mod support;

#[path = "../../aria2-core/tests/fixtures/mock_tracker.rs"]
mod mock_tracker;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::torrent::parser::TorrentMeta;
use base64::Engine as _;
use mock_tracker::MockTrackerServer;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::time::{Duration, Instant};
use support::RunningAria2;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};

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

fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve BT listen port")
        .local_addr()
        .expect("read reserved BT listen port")
        .port()
}

fn multi_file_torrent(tracker_url: &str) -> Vec<u8> {
    let files = [2i64, 8, 1]
        .into_iter()
        .enumerate()
        .map(|(index, length)| {
            BencodeValue::Dict(BTreeMap::from([
                (b"length".to_vec(), BencodeValue::Int(length)),
                (
                    b"path".to_vec(),
                    BencodeValue::List(vec![BencodeValue::Bytes(
                        format!("part-{index}.bin").into_bytes(),
                    )]),
                ),
            ]))
        })
        .collect();
    let info = BTreeMap::from([
        (b"files".to_vec(), BencodeValue::List(files)),
        (
            b"name".to_vec(),
            BencodeValue::Bytes(b"multi-seed".to_vec()),
        ),
        (b"piece length".to_vec(), BencodeValue::Int(4)),
        (
            b"pieces".to_vec(),
            // SHA-1 hashes of b"seed", b"data", and the short final piece
            // b"xyz", matching the concatenated stream across file boundaries.
            BencodeValue::Bytes(vec![
                0x92, 0x71, 0x3d, 0x47, 0x09, 0x37, 0x71, 0x11, 0xcf, 0x31, 0xf2, 0xa7, 0x19, 0x86,
                0xc4, 0x11, 0xbd, 0x6c, 0xb5, 0xb0, 0xa1, 0x7c, 0x9a, 0xaa, 0x61, 0xe8, 0x0a, 0x1b,
                0xf7, 0x1d, 0x0d, 0x85, 0x0a, 0xf4, 0xe5, 0xba, 0xa9, 0x80, 0x0b, 0xbd, 0x66, 0xb2,
                0x74, 0x17, 0xd3, 0x7e, 0x02, 0x4c, 0x46, 0x52, 0x6c, 0x2f, 0x6d, 0x35, 0x8a, 0x75,
                0x4f, 0xc5, 0x52, 0xf3,
            ]),
        ),
    ]);
    BencodeValue::Dict(BTreeMap::from([
        (
            b"announce".to_vec(),
            BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
        ),
        (b"info".to_vec(), BencodeValue::Dict(info)),
    ]))
    .encode()
}

async fn read_bt_message(stream: &mut TcpStream) -> Vec<u8> {
    let length = tokio::time::timeout(Duration::from_secs(5), stream.read_u32())
        .await
        .expect("BT message length timed out")
        .expect("read BT message length");
    if length == 0 {
        return Vec::new();
    }
    assert!(
        length <= 1024 * 1024,
        "unexpectedly large BT frame: {length}"
    );
    let mut message = vec![0; length as usize];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut message))
        .await
        .expect("BT message body timed out")
        .expect("read BT message body");
    message
}

async fn request_piece_block(stream: &mut TcpStream, index: u32, length: u32) -> Vec<u8> {
    let mut request = Vec::with_capacity(17);
    request.extend_from_slice(&13u32.to_be_bytes());
    request.push(6);
    request.extend_from_slice(&index.to_be_bytes());
    request.extend_from_slice(&0u32.to_be_bytes());
    request.extend_from_slice(&length.to_be_bytes());
    stream
        .write_all(&request)
        .await
        .expect("request piece block across file boundary");

    loop {
        let message = read_bt_message(stream).await;
        if message.first() != Some(&7) {
            continue;
        }
        assert_eq!(&message[1..5], &index.to_be_bytes());
        assert_eq!(&message[5..9], &0u32.to_be_bytes());
        return message[9..].to_vec();
    }
}

async fn serve_multi_file_leecher(
    listener: tokio::net::TcpListener,
    info_hash: [u8; 20],
) -> Vec<Vec<u8>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .expect("aria2 did not connect to the controlled leecher")
            .expect("accept aria2 seeding connection");
        let mut request_handshake = [0; 68];
        let handshake = tokio::time::timeout(
            Duration::from_millis(250),
            stream.read_exact(&mut request_handshake),
        )
        .await;
        if matches!(handshake, Ok(Ok(_))) && request_handshake[28..48] == info_hash {
            break stream;
        }

        // This loopback port can receive unrelated local peers. Reject those
        // exactly as a BitTorrent peer rejects a different swarm, then keep
        // listening for the tracker-selected torrent.
        let _ = stream.shutdown().await;
    };

    let mut response_handshake = [0; 68];
    response_handshake[0] = 19;
    response_handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    response_handshake[28..48].copy_from_slice(&info_hash);
    response_handshake[48..68].fill(0x73);
    stream
        .write_all(&response_handshake)
        .await
        .expect("send leecher handshake");
    // Three-piece bitfield: this leecher has no data.
    stream
        .write_all(&[0, 0, 0, 2, 5, 0, 0, 0, 0, 1, 1])
        .await
        .expect("send empty bitfield and unchoke");
    stream
        .write_all(&[0, 0, 0, 1, 2])
        .await
        .expect("send Interested");

    loop {
        let message = read_bt_message(&mut stream).await;
        if message.first() == Some(&1) {
            break;
        }
    }

    let first_piece = request_piece_block(&mut stream, 0, 4).await;
    let short_final_piece = request_piece_block(&mut stream, 2, 3).await;
    vec![first_piece, short_final_piece]
}

async fn next_utp_payload(
    socket: &mut aria2_protocol::bittorrent::utp::UtpSocket,
) -> (u16, Vec<u8>) {
    loop {
        let payloads = socket.poll_recv().expect("poll incoming uTP payloads");
        if let Some(payload) = payloads.into_iter().next() {
            return payload;
        }

        let readiness = socket
            .readiness_socket()
            .expect("get uTP socket readiness handle");
        if let Some(delay) = socket.next_timer_delay() {
            tokio::select! {
                ready = readiness.readable() => ready.expect("wait for uTP socket readability"),
                _ = tokio::time::sleep(delay) => socket.process_timers().expect("process uTP timers"),
            }
        } else {
            readiness
                .readable()
                .await
                .expect("wait for uTP socket readability");
        }
    }
}

async fn next_utp_bt_message(
    socket: &mut aria2_protocol::bittorrent::utp::UtpSocket,
    conn_id: u16,
    buffer: &mut Vec<u8>,
) -> Vec<u8> {
    loop {
        if buffer.len() >= 4 {
            let message_length =
                u32::from_be_bytes(buffer[..4].try_into().expect("four-byte message length"))
                    as usize;
            if message_length == 0 {
                buffer.drain(..4);
                continue;
            }
            if buffer.len() >= message_length + 4 {
                return buffer.drain(..message_length + 4).collect();
            }
        }

        let (received_conn_id, payload) = next_utp_payload(socket).await;
        assert_eq!(received_conn_id, conn_id, "unexpected uTP connection");
        buffer.extend_from_slice(&payload);
    }
}

fn send_utp_bytes(
    socket: &mut aria2_protocol::bittorrent::utp::UtpSocket,
    conn_id: u16,
    bytes: &[u8],
) {
    let sent = socket.send(conn_id, bytes).expect("send bytes over uTP");
    assert_eq!(sent, bytes.len(), "uTP fixture must send the full BT frame");
}

async fn serve_utp_leecher(
    mut socket: aria2_protocol::bittorrent::utp::UtpSocket,
    info_hash: [u8; 20],
) -> Vec<Vec<u8>> {
    use aria2_protocol::bittorrent::message::handshake::Handshake;

    let mut buffer = Vec::new();
    while buffer.len() < 68 {
        let (_, payload) = next_utp_payload(&mut socket).await;
        buffer.extend_from_slice(&payload);
    }
    let request_handshake = Handshake::parse(&buffer[..68]).expect("parse seeder handshake");
    assert_eq!(request_handshake.info_hash, info_hash);
    buffer.drain(..68);

    let conn_id = *socket
        .connection_ids()
        .first()
        .expect("uTP socket admitted the seeder connection");
    let peer_id = [0x73; 20];
    send_utp_bytes(
        &mut socket,
        conn_id,
        &Handshake::new(&info_hash, &peer_id).to_bytes(),
    );
    // Empty three-piece bitfield followed by Interested.
    send_utp_bytes(&mut socket, conn_id, &[0, 0, 0, 2, 5, 0, 0, 0, 0, 1, 2]);

    loop {
        let message = next_utp_bt_message(&mut socket, conn_id, &mut buffer).await;
        if message.get(4) == Some(&1) {
            break;
        }
    }

    let mut received = Vec::with_capacity(2);
    for (piece_index, length) in [(0u32, 4u32), (2, 3)] {
        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&13u32.to_be_bytes());
        request.push(6);
        request.extend_from_slice(&piece_index.to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&length.to_be_bytes());
        send_utp_bytes(&mut socket, conn_id, &request);

        loop {
            let message = next_utp_bt_message(&mut socket, conn_id, &mut buffer).await;
            if message.get(4) != Some(&7) {
                continue;
            }
            assert_eq!(&message[5..9], &piece_index.to_be_bytes());
            assert_eq!(&message[9..13], &0u32.to_be_bytes());
            received.push(message[13..].to_vec());
            break;
        }
    }
    received
}

#[tokio::test]
async fn cli_rpc_seeder_uploads_exact_piece_block_across_multi_file_boundary() {
    let temp = tempfile::tempdir().expect("temporary seed directory");
    let torrent_root = temp.path().join("multi-seed");
    std::fs::create_dir_all(&torrent_root).expect("create multi-file torrent root");
    std::fs::write(torrent_root.join("part-0.bin"), b"se").expect("write first file");
    std::fs::write(torrent_root.join("part-1.bin"), b"eddataxy").expect("write second file");
    std::fs::write(torrent_root.join("part-2.bin"), b"z").expect("write final file");

    let listener = TokioTcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind controlled leecher");
    let leecher_address = listener.local_addr().expect("read leecher address");
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder = multi_file_torrent(&placeholder_tracker.announce_url());
    let info_hash = TorrentMeta::parse(&placeholder)
        .expect("multi-file fixture torrent should parse")
        .info_hash
        .bytes;
    drop(placeholder_tracker);

    let tracker =
        MockTrackerServer::start_with_event_peers(Vec::new(), vec![leecher_address.port()], 1)
            .await;
    let torrent = multi_file_torrent(&tracker.announce_url());
    assert_eq!(
        TorrentMeta::parse(&torrent)
            .expect("final multi-file fixture torrent should parse")
            .info_hash
            .bytes,
        info_hash,
        "changing the tracker URL must not change the info-hash"
    );
    let args = [
        format!("--dir={}", temp.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-dht=false".to_owned(),
        "--enable-utp=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
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

    tracker.wait_for_event("completed").await;
    let peer_task = tokio::spawn(serve_multi_file_leecher(listener, info_hash));
    let received = tokio::time::timeout(Duration::from_secs(12), peer_task)
        .await
        .expect("multi-file upload did not finish")
        .expect("leecher task panicked");

    assert_eq!(
        received[0], b"seed",
        "piece 0 is `se` + `ed` across two files"
    );
    assert_eq!(
        received[1], b"xyz",
        "short final piece 2 crosses from part-1.bin into part-2.bin"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "uploadLength"]
            ]),
        );
        if status["uploadLength"] == "7" {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "upload stats did not converge: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(status["status"], "active");
    assert_eq!(status["completedLength"], "11");
    assert_eq!(status["totalLength"], "11");
    assert_eq!(status["uploadLength"], "7");
}

#[tokio::test]
async fn cli_rpc_seeder_uploads_multi_file_blocks_over_utp() {
    let temp = tempfile::tempdir().expect("temporary seed directory");
    let torrent_root = temp.path().join("multi-seed");
    std::fs::create_dir_all(&torrent_root).expect("create multi-file torrent root");
    std::fs::write(torrent_root.join("part-0.bin"), b"se").expect("write first file");
    std::fs::write(torrent_root.join("part-1.bin"), b"eddataxy").expect("write second file");
    std::fs::write(torrent_root.join("part-2.bin"), b"z").expect("write final file");

    let utp_socket = aria2_protocol::bittorrent::utp::UtpSocket::bind("127.0.0.1:0")
        .expect("bind controlled uTP leecher");
    let peer_port = utp_socket.local_addr().port();
    let placeholder_tracker = MockTrackerServer::start(0).await;
    let placeholder_torrent = multi_file_torrent(&placeholder_tracker.announce_url());
    let info_hash = TorrentMeta::parse(&placeholder_torrent)
        .expect("multi-file fixture torrent should parse")
        .info_hash
        .bytes;
    drop(placeholder_tracker);

    let tracker = MockTrackerServer::start_with_peers(vec![peer_port], false).await;
    let torrent = multi_file_torrent(&tracker.announce_url());
    let args = [
        format!("--dir={}", temp.path().display()),
        format!("--listen-port={}", reserve_loopback_port()),
        "--enable-utp=true".to_owned(),
        "--enable-dht=false".to_owned(),
        "--enable-public-trackers=false".to_owned(),
        "--enable-peer-exchange=false".to_owned(),
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

    tracker.wait_for_event("completed").await;
    let peer_task = tokio::spawn(serve_utp_leecher(utp_socket, info_hash));
    let received = tokio::time::timeout(Duration::from_secs(15), peer_task)
        .await
        .expect("uTP multi-file upload did not finish")
        .expect("uTP leecher task panicked");
    assert_eq!(received, [b"seed".to_vec(), b"xyz".to_vec()]);

    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        let status = rpc(
            &client,
            2,
            "aria2.tellStatus",
            json!([
                gid,
                ["status", "completedLength", "totalLength", "uploadLength"]
            ]),
        );
        if status["uploadLength"] == "7" {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "uTP upload stats did not converge: {status}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(status["status"], "active");
    assert_eq!(status["completedLength"], "11");
    assert_eq!(status["totalLength"], "11");
    assert_eq!(status["uploadLength"], "7");
}
