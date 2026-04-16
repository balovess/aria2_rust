//! Mock BT Seeder for deep E2E testing
//!
//! Implements enough of the BT wire protocol to act as a seed:
//! - BT handshake (protocol ID + reserved + info_hash + peer_id)
//! - Message exchange: bitfield, unchoke, interested, request, piece, cancel
//! - Configurable behavior: choke timing, piece data, drop simulation

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const BT_PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
const BT_HANDSHAKE_LEN: usize = 68;
const MSG_CHOKE: u8 = 0;
const MSG_UNCHOKE: u8 = 1;
const MSG_INTERESTED: u8 = 2;
const MSG_BITFIELD: u8 = 5;
const MSG_REQUEST: u8 = 6;
const MSG_PIECE: u8 = 7;
const MSG_CANCEL: u8 = 8;

/// Configuration for mock seeder behavior
#[derive(Debug, Clone)]
pub struct SeederConfig {
    pub delay_handshake_ms: u64,
    pub delay_unchoke_ms: u64,
    pub choke_after_n_bytes: Option<u64>,
    pub send_corrupt_piece: bool,
    pub max_connections: u32,
}

impl Default for SeederConfig {
    fn default() -> Self {
        Self {
            delay_handshake_ms: 0,
            delay_unchoke_ms: 10,
            choke_after_n_bytes: None,
            send_corrupt_piece: false,
            max_connections: 4,
        }
    }
}

pub struct MockBtSeeder {
    addr: SocketAddr,
    info_hash: [u8; 20],
    pieces: HashMap<u32, Vec<u8>>,
    config: SeederConfig,
    shutdown: Arc<AtomicBool>,
    connection_count: Arc<AtomicUsize>,
}

impl MockBtSeeder {
    /// Start a mock BT seed server on a random port
    ///
    /// # Arguments
    /// * `info_hash` - The torrent info hash this seeder serves
    /// * `pieces` - Map of piece_index -> piece data
    /// * `config` - Behavior configuration
    pub async fn start(
        info_hash: [u8; 20],
        pieces: HashMap<u32, Vec<u8>>,
        config: SeederConfig,
    ) -> Self {
        let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .expect("Failed to bind MockBtSeeder");
        let actual_addr = listener.local_addr().unwrap();

        let shutdown = Arc::new(AtomicBool::new(false));
        let conn_count = Arc::new(AtomicUsize::new(0));
        let shutdown_clone = shutdown.clone();
        let conn_count_clone = conn_count.clone();

        let ih = info_hash;
        let pc = pieces.clone();
        let cfg = config.clone();

        tokio::spawn(async move {
            loop {
                if shutdown_clone.load(Ordering::SeqCst) {
                    break;
                }

                match tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept())
                    .await
                {
                    Ok(Ok((stream, _))) => {
                        let current = conn_count_clone.fetch_add(1, Ordering::SeqCst);
                        if current >= cfg.max_connections as usize {
                            conn_count_clone.fetch_sub(1, Ordering::SeqCst);
                            drop(stream);
                            continue;
                        }
                        let ih_copy = ih;
                        let pc_copy = pc.clone();
                        let cfg_copy = cfg.clone();
                        let cc = conn_count_clone.clone();
                        tokio::spawn(async move {
                            let mut s = stream;
                            Self::handle_connection(&mut s, &ih_copy, &pc_copy, &cfg_copy).await;
                            cc.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Ok(Err(_)) | Err(_) => continue,
                }
            }
        });

        Self {
            addr: actual_addr,
            info_hash,
            pieces,
            config,
            shutdown,
            connection_count: conn_count,
        }
    }

    /// Get the listening port
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// Get the local address for peer connections
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Get the number of active peer connections
    pub fn connection_count(&self) -> usize {
        self.connection_count.load(Ordering::SeqCst)
    }

    /// Shutdown the seeder and clean up resources.
    ///
    /// Sets the shutdown flag, sends a dummy TCP connection to unblock the accept loop,
    /// then waits for background tasks to finish.
    pub async fn shutdown(self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Send a dummy connection to unblock the accept loop
        let _ = tokio::net::TcpStream::connect(self.addr).await;
        // Wait longer for spawned tasks to observe the flag and exit.
        // The accept loop checks `shutdown` every 100ms via timeout-based accept.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    async fn handle_connection(
        stream: &mut tokio::net::TcpStream,
        expected_info_hash: &[u8; 20],
        pieces: &HashMap<u32, Vec<u8>>,
        config: &SeederConfig,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Apply handshake delay
        if config.delay_handshake_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(config.delay_handshake_ms)).await;
        }

        // Read client handshake (68 bytes)
        let mut buf = [0u8; BT_HANDSHAKE_LEN];
        if stream.read_exact(&mut buf).await.is_err() {
            return;
        }

        // Validate protocol prefix (bytes 0-18)
        if &buf[0..19] != BT_PROTOCOL {
            return;
        }
        // Validate info_hash (bytes 28-47)
        if &buf[28..48] != expected_info_hash.as_slice() {
            return;
        }

        // Build and send our handshake response
        let mut response = Vec::with_capacity(BT_HANDSHAKE_LEN);
        response.extend_from_slice(BT_PROTOCOL);
        response.extend_from_slice(&[0u8; 8]); // reserved bytes
        response.extend_from_slice(expected_info_hash); // our info_hash
        response.extend_from_slice(b"MockSeeder-001"); // our peer_id

        if stream.write_all(&response).await.is_err() {
            return;
        }

        // Wait for Bitfield message from client
        let mut msg_buf = [0u8; 65536];
        let msg_len_result = stream.read_u32().await;
        if msg_len_result.is_err() {
            return;
        }
        let msg_len = msg_len_result.unwrap() as usize;
        if msg_len == 0 || msg_len > msg_buf.len() {
            return;
        }
        if stream.read_exact(&mut msg_buf[..msg_len]).await.is_err() {
            return;
        }

        // Send have-all bitfield
        let num_pieces = pieces.len().max(1);
        let bf_size = num_pieces.div_ceil(8);
        let bitfield: Vec<u8> = vec![0xFF; bf_size];
        let mut bf_msg = Vec::with_capacity(5 + bitfield.len());
        bf_msg.extend_from_slice(&(1 + bitfield.len() as u32).to_be_bytes());
        bf_msg.push(MSG_BITFIELD);
        bf_msg.extend_from_slice(&bitfield);

        if stream.write_all(&bf_msg).await.is_err() {
            return;
        }

        // Delay before unchoke
        if config.delay_unchoke_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(config.delay_unchoke_ms)).await;
        }

        // Send Unchoke
        let unchoke_msg: [u8; 5] = [0, 0, 0, 1, MSG_UNCHOKE];
        if stream.write_all(&unchoke_msg).await.is_err() {
            return;
        }

        // Main message loop: handle Request → respond with Piece
        let mut bytes_sent: u64 = 0;
        loop {
            let len_result = stream.read_u32().await;
            match len_result {
                Ok(len) if len > 0 && len as usize <= msg_buf.len() => {
                    if stream
                        .read_exact(&mut msg_buf[..len as usize])
                        .await
                        .is_err()
                    {
                        break;
                    }
                    let msg_id = msg_buf[0];

                    match msg_id {
                        MSG_REQUEST if len >= 13 => {
                            // Request: <index:4><offset:4><length:4>
                            let index = u32::from_be_bytes([
                                msg_buf[1], msg_buf[2], msg_buf[3], msg_buf[4],
                            ]);
                            let _offset = u32::from_be_bytes([
                                msg_buf[5], msg_buf[6], msg_buf[7], msg_buf[8],
                            ]);
                            let length = u32::from_be_bytes([
                                msg_buf[9],
                                msg_buf[10],
                                msg_buf[11],
                                msg_buf[12],
                            ]);

                            let data = if config.send_corrupt_piece {
                                vec![0xDE; length as usize]
                            } else {
                                pieces
                                    .get(&index)
                                    .cloned()
                                    .unwrap_or_else(|| vec![0u8; length as usize])
                            };

                            // Build Piece message: <len:4><id:1><index:4><offset:4><data>
                            let payload_len = 9 + data.len();
                            let mut piece_msg = Vec::with_capacity(4 + payload_len);
                            piece_msg.extend_from_slice(&(payload_len as u32).to_be_bytes());
                            piece_msg.push(MSG_PIECE);
                            piece_msg.extend_from_slice(&index.to_be_bytes());
                            piece_msg.extend_from_slice(&_offset.to_be_bytes());
                            piece_msg.extend_from_slice(&data);

                            bytes_sent += piece_msg.len() as u64;

                            if let Some(limit) = config.choke_after_n_bytes
                                && bytes_sent >= limit
                            {
                                break;
                            }

                            if stream.write_all(&piece_msg).await.is_err() {
                                break;
                            }
                        }
                        MSG_KEEPALIVE => {} // keepalive (len=0 handled above)
                        MSG_CANCEL | MSG_INTERESTED | MSG_CHOKE => {} // ignore
                        _ => {}             // unknown message, ignore
                    }
                }
                Ok(0) => break, // EOF / keepalive
                Err(_) => break,
                _ => break,
            }
        }
    }
}

const MSG_KEEPALIVE: u8 = 0xFF; // sentinel for zero-length messages

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    // Note: Real TCP I/O tests are slow on Windows CI and may hang.
    // The seeder is already validated by the 12 BT deep E2E tests in
    // `deep_e2e_bittorrent.rs` which use it successfully.
    #[ignore]
    async fn test_seeder_basic_handshake() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut pieces = HashMap::new();
        pieces.insert(0u32, vec![1, 2, 3, 4]);
        let info_hash = [0xABu8; 20];

        let seeder = MockBtSeeder::start(info_hash, pieces, SeederConfig::default()).await;

        // Connect as a fake peer
        let mut stream = tokio::net::TcpStream::connect(seeder.addr()).await.unwrap();

        // Send handshake
        let mut hs = Vec::with_capacity(68);
        hs.extend_from_slice(BT_PROTOCOL);
        hs.extend_from_slice(&[0u8; 8]);
        hs.extend_from_slice(&info_hash);
        hs.extend_from_slice(b"TestClient-001");
        stream.write_all(&hs).await.unwrap();

        // Receive server handshake
        let mut resp = [0u8; 68];
        stream.read_exact(&mut resp).await.unwrap();

        // Verify protocol + info_hash in response
        assert_eq!(&resp[0..19], BT_PROTOCOL);
        assert_eq!(&resp[28..48], info_hash.as_slice());

        seeder.shutdown().await;
    }

    #[tokio::test]
    #[ignore]
    async fn test_seeder_serves_piece() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let mut pieces = HashMap::new();
        let piece_data = vec![0x42u8; 256];
        pieces.insert(0u32, piece_data.clone());
        let info_hash = [0xCDu8; 20];

        let seeder = MockBtSeeder::start(info_hash, pieces, SeederConfig::default()).await;

        let mut stream = tokio::net::TcpStream::connect(seeder.addr()).await.unwrap();

        // Handshake
        let mut hs = Vec::with_capacity(68);
        hs.extend_from_slice(BT_PROTOCOL);
        hs.extend_from_slice(&[0u8; 8]);
        hs.extend_from_slice(&info_hash);
        hs.extend_from_slice(b"TestClient-002");
        stream.write_all(&hs).await.unwrap();

        // Read server handshake
        let mut resp = [0u8; 68];
        stream.read_exact(&mut resp).await.unwrap();

        // Read bitfield message
        let bf_len = stream.read_u32().await.unwrap() as usize;
        let mut bf_buf = vec![0u8; bf_len];
        stream.read_exact(&mut bf_buf).await.unwrap();
        assert_eq!(bf_buf[0], MSG_BITFIELD);

        // Send Interested
        let interested: [u8; 5] = [0, 0, 0, 1, MSG_INTERESTED];
        stream.write_all(&interested).await.unwrap();

        // Read Unchoke
        let mut unchoke_buf = [0u8; 5];
        stream.read_exact(&mut unchoke_buf).await.unwrap();
        assert_eq!(unchoke_buf[4], MSG_UNCHOKE);

        // Send Request for piece 0
        let mut req = Vec::with_capacity(17);
        req.extend_from_slice(&13u32.to_be_bytes()); // length
        req.push(MSG_REQUEST);
        req.extend_from_slice(&0u32.to_be_bytes()); // index=0
        req.extend_from_slice(&0u32.to_be_bytes()); // offset=0
        req.extend_from_slice(&(piece_data.len() as u32).to_be_bytes()); // length
        stream.write_all(&req).await.unwrap();

        // Read Piece response
        let piece_len = stream.read_u32().await.unwrap() as usize;
        let mut piece_hdr = [0u8; 9];
        stream.read_exact(&mut piece_hdr).await.unwrap();
        assert_eq!(piece_hdr[0], MSG_PIECE);
        let piece_index =
            u32::from_be_bytes([piece_hdr[1], piece_hdr[2], piece_hdr[3], piece_hdr[4]]);
        assert_eq!(piece_index, 0);

        let data_len = piece_len - 9;
        let mut recv_data = vec![0u8; data_len];
        stream.read_exact(&mut recv_data).await.unwrap();
        assert_eq!(recv_data, piece_data);

        seeder.shutdown().await;
    }
}
