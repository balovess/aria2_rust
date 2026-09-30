use super::support::RunningAria2;
use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    net::{SocketAddr, TcpListener},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener as TokioTcpListener, TcpStream},
    sync::Notify,
    task::{JoinHandle, JoinSet},
};

pub const PIECE_LENGTH: usize = 512 * 1024;
pub const BLOCK_LENGTH: usize = 16 * 1024;
pub const DEFAULT_BURST_LENGTH: usize = 256 * 1024;
pub const UPLOAD_RATE_BYTES_PER_SEC: usize = 64 * 1024;
#[allow(dead_code)]
pub const UPLOAD_RATE_KIB: usize = UPLOAD_RATE_BYTES_PER_SEC / 1024;

pub fn rpc(client: &RunningAria2, id: u64, method: &str, params: Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
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

pub fn upload_rate_torrent(tracker_url: &str, name: &str) -> Vec<u8> {
    let mut info = BTreeMap::new();
    info.insert(
        b"length".to_vec(),
        BencodeValue::Int((PIECE_LENGTH * 2) as i64),
    );
    info.insert(
        b"name".to_vec(),
        BencodeValue::Bytes(name.as_bytes().to_vec()),
    );
    info.insert(
        b"piece length".to_vec(),
        BencodeValue::Int(PIECE_LENGTH as i64),
    );
    let mut pieces = Vec::with_capacity(40);
    pieces.extend_from_slice(&[
        0xce, 0x17, 0x77, 0xcf, 0xbf, 0x97, 0xb7, 0xd3, 0xc9, 0x06, 0x66, 0xdd, 0x7c, 0xd4, 0xd2,
        0x8f, 0x80, 0x9f, 0xca, 0x83,
    ]);
    pieces.extend_from_slice(&[
        0x77, 0x58, 0x1f, 0x82, 0x00, 0x2c, 0xbd, 0x2e, 0x06, 0x3f, 0xcd, 0xad, 0x85, 0x96, 0x29,
        0x95, 0x8d, 0xf8, 0x1f, 0x23,
    ]);
    info.insert(b"pieces".to_vec(), BencodeValue::Bytes(pieces));

    let mut root = BTreeMap::new();
    root.insert(
        b"announce".to_vec(),
        BencodeValue::Bytes(tracker_url.as_bytes().to_vec()),
    );
    root.insert(b"info".to_vec(), BencodeValue::Dict(info));
    BencodeValue::Dict(root).encode()
}

pub struct PartialSeeder {
    pub addr: SocketAddr,
    release_tail: Arc<Notify>,
    task: JoinHandle<()>,
}

impl PartialSeeder {
    pub async fn start(info_hash: [u8; 20], payload: Arc<Vec<u8>>) -> Self {
        let listener = TokioTcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind controlled partial seeder");
        let addr = listener.local_addr().expect("seeder listener address");
        let release_tail = Arc::new(Notify::new());
        let release_tail_task = Arc::clone(&release_tail);
        let task = tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        connections.spawn(serve_peer(
                            stream,
                            info_hash,
                            Arc::clone(&payload),
                            Arc::clone(&release_tail_task),
                        ));
                    }
                    Some(_) = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            addr,
            release_tail,
            task,
        }
    }

    pub fn release_tail(&self) {
        self.release_tail.notify_one();
    }
}

impl Drop for PartialSeeder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_peer(
    mut stream: TcpStream,
    info_hash: [u8; 20],
    payload: Arc<Vec<u8>>,
    release_tail: Arc<Notify>,
) -> io::Result<()> {
    let mut request_handshake = [0u8; 68];
    tokio::time::timeout(
        Duration::from_secs(5),
        stream.read_exact(&mut request_handshake),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "peer handshake timed out"))??;
    if request_handshake[0] != 19 || request_handshake[28..48] != info_hash {
        return Ok(());
    }

    let mut response = [0u8; 68];
    response[0] = 19;
    response[1..20].copy_from_slice(b"BitTorrent protocol");
    response[28..48].copy_from_slice(&info_hash);
    response[48..68].fill(0x53);
    stream.write_all(&response).await?;
    stream.write_all(&[0, 0, 0, 2, 5, 0x80]).await?;
    stream.write_all(&[0, 0, 0, 1, 1]).await?;
    stream.write_all(&[0, 0, 0, 1, 2]).await?;

    let mut tail_available = false;
    loop {
        let message = tokio::select! {
            message = read_peer_message(&mut stream) => match message? {
                Some(message) => Some(message),
                None => return Ok(()),
            },
            _ = release_tail.notified(), if !tail_available => {
                stream.write_all(&[0, 0, 0, 5, 4, 0, 0, 0, 1]).await?;
                tail_available = true;
                None
            }
        };
        let Some(message) = message else {
            continue;
        };
        if message.first() != Some(&6) || message.len() != 13 {
            continue;
        }

        let index = u32::from_be_bytes(message[1..5].try_into().unwrap());
        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
        let length = u32::from_be_bytes(message[9..13].try_into().unwrap()) as usize;
        if index > u32::from(tail_available)
            || length == 0
            || length > BLOCK_LENGTH
            || begin + length > PIECE_LENGTH
        {
            continue;
        }

        let start = index as usize * PIECE_LENGTH + begin;
        let end = start + length;
        let Some(data) = payload.get(start..end) else {
            continue;
        };
        let mut piece = Vec::with_capacity(13 + length);
        piece.extend_from_slice(&((9 + length) as u32).to_be_bytes());
        piece.push(7);
        piece.extend_from_slice(&index.to_be_bytes());
        piece.extend_from_slice(&(begin as u32).to_be_bytes());
        piece.extend_from_slice(data);
        stream.write_all(&piece).await?;
    }
}

pub async fn connect_interested_leecher(listen_port: u16, info_hash: [u8; 20]) -> TcpStream {
    let mut leecher = TcpStream::connect(("127.0.0.1", listen_port))
        .await
        .expect("connect a controlled leecher to the BT listener");
    let mut handshake = [0u8; 68];
    handshake[0] = 19;
    handshake[1..20].copy_from_slice(b"BitTorrent protocol");
    handshake[28..48].copy_from_slice(&info_hash);
    handshake[48..68].fill(0x69);
    leecher
        .write_all(&handshake)
        .await
        .expect("send BT handshake");
    let mut response = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(5), leecher.read_exact(&mut response))
        .await
        .expect("incoming handshake response timed out")
        .expect("read incoming handshake response");
    assert_eq!(&response[28..48], &info_hash);
    let bitfield = tokio::time::timeout(Duration::from_secs(2), read_peer_message(&mut leecher))
        .await
        .expect("incoming bitfield timed out")
        .expect("read incoming peer bitfield")
        .expect("incoming peer disconnected before sending its bitfield");
    assert_eq!(bitfield, [5, 0x80], "only piece zero is verified locally");
    leecher
        .write_all(&[0, 0, 0, 1, 2])
        .await
        .expect("send Interested after the bitfield");

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let message = read_peer_message(&mut leecher)
                .await
                .expect("read incoming actor message")
                .expect("incoming actor disconnected before unchoking");
            if message.first() == Some(&1) {
                break;
            }
        }
    })
    .await
    .expect("incoming leecher was not unchoked");
    leecher
}

pub async fn request_piece_blocks(leecher: &mut TcpStream, piece_index: u32) {
    for block in 0..PIECE_LENGTH / BLOCK_LENGTH {
        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&13u32.to_be_bytes());
        request.push(6);
        request.extend_from_slice(&piece_index.to_be_bytes());
        request.extend_from_slice(&((block * BLOCK_LENGTH) as u32).to_be_bytes());
        request.extend_from_slice(&(BLOCK_LENGTH as u32).to_be_bytes());
        leecher
            .write_all(&request)
            .await
            .expect("request verified piece block");
    }
}

pub async fn receive_piece(leecher: &mut TcpStream, piece_index: u32) -> Vec<u8> {
    let block_count = PIECE_LENGTH / BLOCK_LENGTH;
    let mut received = vec![0; PIECE_LENGTH];
    let mut seen_blocks = vec![false; block_count];
    let mut completed_blocks = 0;
    while completed_blocks < block_count {
        let message = read_peer_message(leecher)
            .await
            .expect("read uploaded block")
            .expect("incoming actor disconnected during upload");
        if message.first() != Some(&7) {
            continue;
        }
        assert_eq!(&message[1..5], &piece_index.to_be_bytes());
        let begin = u32::from_be_bytes(message[5..9].try_into().unwrap()) as usize;
        let data = &message[9..];
        assert_eq!(data.len(), BLOCK_LENGTH);
        assert_eq!(begin % BLOCK_LENGTH, 0);
        let block_index = begin / BLOCK_LENGTH;
        assert!(block_index < block_count);
        assert!(
            !seen_blocks[block_index],
            "duplicate block at offset {begin}"
        );
        received[begin..begin + data.len()].copy_from_slice(data);
        seen_blocks[block_index] = true;
        completed_blocks += 1;
    }
    received
}

pub async fn read_peer_message(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut length_bytes = [0u8; 4];
    match stream.read_exact(&mut length_bytes).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let length = u32::from_be_bytes(length_bytes) as usize;
    if length == 0 {
        return Ok(Some(Vec::new()));
    }
    if length > 64 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "peer message exceeds the test fixture limit",
        ));
    }
    let mut message = vec![0; length];
    stream.read_exact(&mut message).await?;
    Ok(Some(message))
}
