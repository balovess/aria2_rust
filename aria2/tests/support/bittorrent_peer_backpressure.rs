#![allow(dead_code)]

use super::support::RunningAria2;
use aria2_core::checksum::message_digest::{HashType, MessageDigest};
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};
use tokio::sync::{Notify, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

pub const PIECE_LENGTH: usize = 16 * 1024 * 1024;
pub const BLOCK_LENGTH: usize = 16 * 1024;

pub fn rpc(client: &RunningAria2, id: u64, method: &str, params: Value) -> Value {
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

pub fn reserve_loopback_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("reserve an ephemeral BitTorrent listen port")
        .local_addr()
        .expect("bound socket has a local address")
        .port()
}

pub fn torrent(tracker_url: &str, payload: &[u8]) -> Vec<u8> {
    torrent_with_piece_length(tracker_url, payload, PIECE_LENGTH)
}

pub fn torrent_with_piece_length(
    tracker_url: &str,
    payload: &[u8],
    piece_length: usize,
) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(b"length".to_vec(), BencodeValue::Int(payload.len() as i64));
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(b"backpressure.bin".to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(piece_length as i64),
    );
    let piece_hashes = payload
        .chunks(piece_length)
        .flat_map(|piece| MessageDigest::hash_data(HashType::Sha1, piece))
        .collect::<Vec<_>>();
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(piece_hashes));
    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

#[derive(Clone)]
pub enum PeerMode {
    Idle,
    StopReadingAfterRequestFlood,
    ReadOneUpload,
    ServePiece {
        payload: Arc<Vec<u8>>,
        piece_count: usize,
        corrupt: bool,
        response_delay: Duration,
        hold_initial_requests: usize,
        held_batch_ready: Option<Arc<Notify>>,
        release_held_batch: Option<Arc<Semaphore>>,
        wait_for_first_request: Option<Arc<Notify>>,
    },
}

pub struct FixturePeer {
    pub addr: SocketAddr,
    pub completed_handshakes: Arc<AtomicUsize>,
    pub requests_sent: Arc<AtomicUsize>,
    pub block_requests_received: Arc<AtomicUsize>,
    pub uploaded_bytes: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

struct FixturePeerCounters {
    completed_handshakes: Arc<AtomicUsize>,
    requests_sent: Arc<AtomicUsize>,
    block_requests_received: Arc<AtomicUsize>,
    uploaded_bytes: Arc<AtomicUsize>,
}

impl FixturePeer {
    pub async fn start(info_hash: [u8; 20], peer_id: [u8; 20], mode: PeerMode) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback BitTorrent peer");
        let addr = listener.local_addr().expect("peer listener address");
        let counters = Arc::new(FixturePeerCounters {
            completed_handshakes: Arc::new(AtomicUsize::new(0)),
            requests_sent: Arc::new(AtomicUsize::new(0)),
            block_requests_received: Arc::new(AtomicUsize::new(0)),
            uploaded_bytes: Arc::new(AtomicUsize::new(0)),
        });
        let requests_sent = Arc::clone(&counters.requests_sent);
        let block_requests_received = Arc::clone(&counters.block_requests_received);
        let uploaded_bytes = Arc::clone(&counters.uploaded_bytes);
        let completed_handshakes = Arc::clone(&counters.completed_handshakes);
        let task = tokio::spawn(async move {
            let mut sessions = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        sessions.spawn(serve_peer(
                            stream,
                            info_hash,
                            peer_id,
                            mode.clone(),
                            Arc::clone(&counters),
                        ));
                    }
                    Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            completed_handshakes,
            requests_sent,
            block_requests_received,
            uploaded_bytes,
            task,
        }
    }
}

impl Drop for FixturePeer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    peer_id: [u8; 20],
    mode: PeerMode,
    counters: Arc<FixturePeerCounters>,
) {
    let mut held_piece_requests = Vec::new();
    let mut held_corrupt_batch_released = false;
    let mut request_handshake = [0u8; 68];
    if tokio::time::timeout(
        Duration::from_secs(5),
        stream.read_exact(&mut request_handshake),
    )
    .await
    .is_err()
        || request_handshake[28..48] != info_hash
    {
        return;
    }
    if stream
        .write_all(&Handshake::new(&info_hash, &peer_id).to_bytes())
        .await
        .is_err()
    {
        return;
    }
    counters.completed_handshakes.fetch_add(1, Ordering::SeqCst);
    let mut peer = PeerConnection::from_stream_with_peer(stream, peer_id, false, false);
    let bitfield = match &mode {
        PeerMode::ServePiece { piece_count, .. } => {
            let mut bitfield = vec![0; piece_count.div_ceil(8)];
            for piece_index in 0..*piece_count {
                bitfield[piece_index / 8] |= 1 << (7 - piece_index % 8);
            }
            bitfield
        }
        _ => vec![0],
    };
    if peer
        .send_message(&BtMessage::Bitfield { data: bitfield })
        .await
        .is_err()
    {
        return;
    }
    if !matches!(&mode, PeerMode::Idle) && peer.send_message(&BtMessage::Interested).await.is_err()
    {
        return;
    }

    loop {
        let message = match tokio::time::timeout(Duration::from_secs(20), peer.read_message()).await
        {
            Ok(Ok(Some(message))) => message,
            _ => return,
        };
        match (&mode, message) {
            (PeerMode::StopReadingAfterRequestFlood, BtMessage::Unchoke) => {
                for begin in (0..PIECE_LENGTH).step_by(BLOCK_LENGTH) {
                    let request = PieceBlockRequest::new(0, begin as u32, BLOCK_LENGTH as u32);
                    if peer
                        .send_message(&BtMessage::Request { request })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    counters.requests_sent.fetch_add(1, Ordering::SeqCst);
                }
                std::future::pending::<()>().await;
            }
            (PeerMode::ReadOneUpload, BtMessage::Unchoke) => {
                let request = PieceBlockRequest::new(0, 0, BLOCK_LENGTH as u32);
                if peer
                    .send_message(&BtMessage::Request { request })
                    .await
                    .is_err()
                {
                    return;
                }
                loop {
                    let Ok(Ok(Some(BtMessage::Piece { data, .. }))) =
                        tokio::time::timeout(Duration::from_secs(10), peer.read_message()).await
                    else {
                        continue;
                    };
                    counters
                        .uploaded_bytes
                        .fetch_add(data.len(), Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
            }
            (
                PeerMode::ServePiece {
                    wait_for_first_request,
                    ..
                },
                BtMessage::Interested,
            ) => {
                if let Some(request_started) = wait_for_first_request {
                    request_started.notified().await;
                }
                if peer.send_message(&BtMessage::Unchoke).await.is_err() {
                    return;
                }
            }
            (
                PeerMode::ServePiece {
                    payload,
                    corrupt,
                    response_delay,
                    hold_initial_requests,
                    held_batch_ready,
                    release_held_batch,
                    ..
                },
                BtMessage::Request { request },
            ) => {
                counters
                    .block_requests_received
                    .fetch_add(1, Ordering::SeqCst);
                if *corrupt && !held_corrupt_batch_released && *hold_initial_requests > 0 {
                    held_piece_requests.push(request);
                    if held_piece_requests.len() == *hold_initial_requests {
                        if let Some(batch_ready) = held_batch_ready {
                            batch_ready.notify_one();
                        }
                        if let Some(release) = release_held_batch {
                            let _permit = release.acquire().await;
                        }
                        for held_request in held_piece_requests.drain(..) {
                            let start = held_request.begin as usize;
                            let end = start.saturating_add(held_request.length as usize);
                            let Some(block) = payload.get(start..end) else {
                                return;
                            };
                            let mut data = block.to_vec();
                            if !data.is_empty() {
                                data[0] ^= 0x01;
                            }
                            if peer
                                .send_message(&BtMessage::Piece {
                                    index: held_request.index,
                                    begin: held_request.begin,
                                    data: data.into(),
                                })
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        held_corrupt_batch_released = true;
                    }
                    continue;
                }
                let start = request.begin as usize;
                let end = start.saturating_add(request.length as usize);
                let Some(block) = payload.get(start..end) else {
                    return;
                };
                let mut data = block.to_vec();
                if *corrupt && !data.is_empty() {
                    data[0] ^= 0x01;
                }
                if !response_delay.is_zero() {
                    tokio::time::sleep(*response_delay).await;
                }
                if peer
                    .send_message(&BtMessage::Piece {
                        index: request.index,
                        begin: request.begin,
                        data: data.into(),
                    })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            _ => {}
        }
    }
}
