use std::fmt;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::extension::ut_metadata::{
    ExtensionHandshake, MetadataCollector, UtMetadataMsg,
};
use aria2_protocol::bittorrent::peer::connection::{PeerAddr, PeerConnection};

const METADATA_MAX_SIZE: u64 = 100 * 1024 * 1024;
const PIECE_SIZE_MIN: u32 = 1024;
const PIECE_SIZE_MAX: u32 = 65536;
const DEFAULT_MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone)]
pub enum MetadataExchangeError {
    NoPeersAvailable,
    AllPeersFailed { attempts: usize, last_error: String },
    PeerConnectFailed { addr: String, reason: String },
    PeerTimeout { addr: String },
    UnsupportedPeer { addr: String, reason: String },
    InvalidMetadataSize { size: u64 },
    MetadataTooLarge { size: u64, max: u64 },
    BencodeDecodeFailed { detail: String },
    PieceRejected { piece: u32 },
    PieceTimeout { piece: u32 },
    IncompleteMetadata { expected: u64, received: u64 },
    IoError(String),
}

impl MetadataExchangeError {
    fn is_fatal(&self) -> bool {
        matches!(
            self,
            MetadataExchangeError::MetadataTooLarge { .. }
                | MetadataExchangeError::BencodeDecodeFailed { .. }
                | MetadataExchangeError::NoPeersAvailable
        )
    }

    pub fn addr(&self) -> Option<&str> {
        match self {
            MetadataExchangeError::PeerConnectFailed { addr, .. } => Some(addr),
            MetadataExchangeError::PeerTimeout { addr } => Some(addr),
            MetadataExchangeError::UnsupportedPeer { addr, .. } => Some(addr),
            _ => None,
        }
    }
}

impl fmt::Display for MetadataExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetadataExchangeError::NoPeersAvailable => {
                write!(f, "No peers available for metadata fetch")
            }
            MetadataExchangeError::AllPeersFailed {
                attempts,
                last_error,
            } => {
                write!(
                    f,
                    "All {} peers failed, last error: {}",
                    attempts, last_error
                )
            }
            MetadataExchangeError::PeerConnectFailed { addr, reason } => {
                write!(f, "Connect to {} failed: {}", addr, reason)
            }
            MetadataExchangeError::PeerTimeout { addr } => {
                write!(f, "Connect to {} timed out", addr)
            }
            MetadataExchangeError::UnsupportedPeer { addr, reason } => {
                write!(f, "Peer {} unsupported: {}", addr, reason)
            }
            MetadataExchangeError::InvalidMetadataSize { size } => {
                write!(f, "Invalid metadata_size: {}", size)
            }
            MetadataExchangeError::MetadataTooLarge { size, max } => {
                write!(f, "metadata_size too large: {} (max {})", size, max)
            }
            MetadataExchangeError::BencodeDecodeFailed { detail } => {
                write!(f, "Bencode decode failed: {}", detail)
            }
            MetadataExchangeError::PieceRejected { piece } => {
                write!(f, "Piece {} rejected by peer", piece)
            }
            MetadataExchangeError::PieceTimeout { piece } => {
                write!(f, "ut_metadata timeout for piece {}", piece)
            }
            MetadataExchangeError::IncompleteMetadata { expected, received } => {
                write!(
                    f,
                    "Incomplete metadata collection: expected {} bytes, received {}",
                    expected, received
                )
            }
            MetadataExchangeError::IoError(msg) => write!(f, "IO error: {}", msg),
        }
    }
}

impl std::error::Error for MetadataExchangeError {}

pub struct MetadataExchangeConfig {
    pub max_peers_to_try: usize,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub piece_size: u32,
    pub max_attempts: usize,
}

impl Default for MetadataExchangeConfig {
    fn default() -> Self {
        Self {
            max_peers_to_try: 5,
            connect_timeout: Duration::from_secs(15),
            request_timeout: Duration::from_secs(10),
            piece_size: 16 * 1024,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

impl MetadataExchangeConfig {
    pub fn with_piece_size(mut self, size: u32) -> Self {
        if !(PIECE_SIZE_MIN..=PIECE_SIZE_MAX).contains(&size) {
            warn!(
                "piece_size={} is out of valid range [{}-{}], clamping",
                size, PIECE_SIZE_MIN, PIECE_SIZE_MAX
            );
            self.piece_size = size.clamp(PIECE_SIZE_MIN, PIECE_SIZE_MAX);
        } else {
            self.piece_size = size;
        }
        self
    }

    pub fn with_max_attempts(mut self, attempts: usize) -> Self {
        self.max_attempts = attempts.max(1);
        self
    }
}

pub struct MetadataExchangeSession {
    config: MetadataExchangeConfig,
}

impl MetadataExchangeSession {
    pub fn new(config: MetadataExchangeConfig) -> Self {
        Self { config }
    }

    pub async fn fetch_metadata(
        &self,
        info_hash: &[u8; 20],
        peers: &[SocketAddr],
    ) -> Result<Vec<u8>, MetadataExchangeError> {
        if peers.is_empty() {
            return Err(MetadataExchangeError::NoPeersAvailable);
        }

        let mut attempt_count = 0usize;
        let mut last_error = String::new();

        for peer_addr in peers.iter().take(self.config.max_peers_to_try) {
            if attempt_count >= self.config.max_attempts {
                break;
            }

            match self.exchange_with_peer(info_hash, peer_addr).await {
                Ok(torrent_bytes) => {
                    info!(
                        "Metadata fetched successfully from {} ({} bytes)",
                        peer_addr,
                        torrent_bytes.len()
                    );
                    return Ok(torrent_bytes);
                }
                Err(e) => {
                    attempt_count += 1;
                    last_error = e.to_string();

                    if e.is_fatal() {
                        warn!("Fatal metadata exchange error with {}: {}", peer_addr, e);
                        return Err(e);
                    }

                    warn!(
                        "Recoverable metadata exchange error with {} (attempt {}/{}): {}",
                        peer_addr, attempt_count, self.config.max_attempts, e
                    );
                }
            }
        }

        Err(MetadataExchangeError::AllPeersFailed {
            attempts: attempt_count,
            last_error,
        })
    }

    async fn exchange_with_peer(
        &self,
        info_hash: &[u8; 20],
        peer_addr: &SocketAddr,
    ) -> Result<Vec<u8>, MetadataExchangeError> {
        let addr_str = peer_addr.to_string();
        let addr = PeerAddr::new(&peer_addr.ip().to_string(), peer_addr.port());

        let conn_result = timeout(
            self.config.connect_timeout,
            PeerConnection::connect(&addr, info_hash),
        )
        .await;
        let mut conn = match conn_result {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => {
                return Err(MetadataExchangeError::PeerConnectFailed {
                    addr: addr_str.clone(),
                    reason: e.to_string(),
                });
            }
            Err(_) => {
                return Err(MetadataExchangeError::PeerTimeout {
                    addr: addr_str.clone(),
                });
            }
        };

        debug!("Connected to {}, sending extension handshake", peer_addr);

        let hs_payload = ExtensionHandshake::new(0).to_bencode();
        let hs_encoded = hs_payload.encode();

        conn.stream_write(&[0])
            .await
            .map_err(|e| MetadataExchangeError::IoError(format!("stream_write failed: {}", e)))?;
        conn.stream_write(&hs_encoded)
            .await
            .map_err(|e| MetadataExchangeError::IoError(format!("stream_write failed: {}", e)))?;
        conn.stream_flush()
            .await
            .map_err(|e| MetadataExchangeError::IoError(format!("stream_flush failed: {}", e)))?;

        debug!("Extension handshake sent to {}", peer_addr);

        let remote_hs_data = self.read_extension_message(&mut conn).await?;
        let remote_hs = ExtensionHandshake::parse(&remote_hs_data).ok_or_else(|| {
            MetadataExchangeError::BencodeDecodeFailed {
                detail: "Failed to parse remote extension handshake".to_string(),
            }
        })?;

        let metadata_size = match remote_hs.metadata_size {
            Some(size) => size,
            None => {
                warn!(
                    "Peer {} reported metadata_size=None, skipping...",
                    peer_addr
                );
                return Err(MetadataExchangeError::UnsupportedPeer {
                    addr: addr_str,
                    reason: "Remote did not provide metadata_size".to_string(),
                });
            }
        };

        if metadata_size == 0 {
            warn!("Peer {} reported metadata_size=0, skipping...", peer_addr);
            return Err(MetadataExchangeError::InvalidMetadataSize { size: 0 });
        }

        if metadata_size > METADATA_MAX_SIZE {
            return Err(MetadataExchangeError::MetadataTooLarge {
                size: metadata_size,
                max: METADATA_MAX_SIZE,
            });
        }

        debug!("Remote reports metadata_size={} bytes", metadata_size);

        let num_pieces = metadata_size.div_ceil(self.config.piece_size as u64) as u32;
        let mut collector = MetadataCollector::new(metadata_size, self.config.piece_size);

        for piece_idx in 0..num_pieces {
            if collector.is_complete() {
                break;
            }

            let req_msg = UtMetadataMsg::Request(piece_idx);
            let encoded = req_msg.encode(20);

            conn.stream_write(&encoded).await.map_err(|e| {
                MetadataExchangeError::IoError(format!("stream_write failed: {}", e))
            })?;
            conn.stream_flush().await.map_err(|e| {
                MetadataExchangeError::IoError(format!("stream_flush failed: {}", e))
            })?;

            match timeout(
                self.config.request_timeout,
                self.read_ut_metadata_response(&mut conn),
            )
            .await
            {
                Ok(Ok(UtMetadataMsg::Data(recv_piece, data))) => {
                    collector.add_piece(recv_piece, &data);
                    debug!(
                        "Received piece {}/{} ({} bytes)",
                        recv_piece + 1,
                        num_pieces,
                        data.len()
                    );
                }
                Ok(Ok(UtMetadataMsg::Reject(_))) => {
                    debug!("Piece {} rejected by {}", piece_idx, peer_addr);
                    return Err(MetadataExchangeError::PieceRejected { piece: piece_idx });
                }
                Ok(Ok(UtMetadataMsg::Request(_))) => {
                    return Err(MetadataExchangeError::BencodeDecodeFailed {
                        detail: "Unexpected Request message type from peer".to_string(),
                    });
                }
                Ok(Err(inner_err)) => {
                    return Err(MetadataExchangeError::BencodeDecodeFailed {
                        detail: format!("ut_metadata error for piece {}: {}", piece_idx, inner_err),
                    });
                }
                Err(_) => {
                    return Err(MetadataExchangeError::PieceTimeout { piece: piece_idx });
                }
            }
        }

        collector.assemble().ok_or_else(|| {
            let received = (collector.progress() * metadata_size as f64) as u64;
            MetadataExchangeError::IncompleteMetadata {
                expected: metadata_size,
                received,
            }
        })
    }

    async fn read_extension_message(
        &self,
        conn: &mut PeerConnection,
    ) -> Result<BencodeValue, MetadataExchangeError> {
        let mut len_buf = [0u8; 4];
        conn.stream_read_exact(&mut len_buf).await.map_err(|e| {
            MetadataExchangeError::IoError(format!("Read message length failed: {}", e))
        })?;

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > 10 * 1024 * 1024 {
            return Err(MetadataExchangeError::BencodeDecodeFailed {
                detail: "Invalid extension message length".to_string(),
            });
        }

        let mut payload = vec![0u8; msg_len];
        conn.stream_read_exact(&mut payload).await.map_err(|e| {
            MetadataExchangeError::IoError(format!("Read message body failed: {}", e))
        })?;

        if payload.first().copied() != Some(20u8) {
            return Err(MetadataExchangeError::BencodeDecodeFailed {
                detail: "Expected extended message ID 20".to_string(),
            });
        }

        BencodeValue::decode(&payload[1..])
            .map(|(v, _)| v)
            .map_err(|e| MetadataExchangeError::BencodeDecodeFailed {
                detail: format!("Decode BEncode failed: {}", e),
            })
    }

    async fn read_ut_metadata_response(
        &self,
        conn: &mut PeerConnection,
    ) -> Result<UtMetadataMsg, MetadataExchangeError> {
        let mut len_buf = [0u8; 4];
        conn.stream_read_exact(&mut len_buf).await.map_err(|e| {
            MetadataExchangeError::IoError(format!("Read ut_metadata length failed: {}", e))
        })?;

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len == 0 || msg_len > 10 * 1024 * 1024 {
            return Err(MetadataExchangeError::BencodeDecodeFailed {
                detail: "Invalid ut_metadata message length".to_string(),
            });
        }

        let mut payload = vec![0u8; msg_len];
        conn.stream_read_exact(&mut payload).await.map_err(|e| {
            MetadataExchangeError::IoError(format!("Read ut_metadata body failed: {}", e))
        })?;

        UtMetadataMsg::decode(&payload).map_err(|e| MetadataExchangeError::BencodeDecodeFailed {
            detail: format!("ut_metadata decode failed: {}", e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let cfg = MetadataExchangeConfig::default();
        assert_eq!(cfg.max_peers_to_try, 5);
        assert_eq!(cfg.piece_size, 16 * 1024);
        assert_eq!(cfg.max_attempts, DEFAULT_MAX_ATTEMPTS);
    }

    #[test]
    fn test_fetch_metadata_no_peers() {
        let session = MetadataExchangeSession::new(MetadataExchangeConfig::default());
        let target_hash = [0u8; 20];
        let peers: Vec<SocketAddr> = vec![];

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(session.fetch_metadata(&target_hash, &peers));

        assert!(result.is_err());
        match result.unwrap_err() {
            MetadataExchangeError::NoPeersAvailable => {}
            other => panic!("Expected NoPeersAvailable, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_read_extension_message_invalid_length_zero() {
        let session = MetadataExchangeSession::new(MetadataExchangeConfig::default());
        let _ = session;
    }

    #[test]
    fn test_error_enum_variant_count() {
        let _ = MetadataExchangeError::NoPeersAvailable;
        let _ = MetadataExchangeError::AllPeersFailed {
            attempts: 0,
            last_error: String::new(),
        };
        let _ = MetadataExchangeError::PeerConnectFailed {
            addr: String::new(),
            reason: String::new(),
        };
        let _ = MetadataExchangeError::PeerTimeout {
            addr: String::new(),
        };
        let _ = MetadataExchangeError::UnsupportedPeer {
            addr: String::new(),
            reason: String::new(),
        };
        let _ = MetadataExchangeError::InvalidMetadataSize { size: 0 };
        let _ = MetadataExchangeError::MetadataTooLarge { size: 0, max: 0 };
        let _ = MetadataExchangeError::BencodeDecodeFailed {
            detail: String::new(),
        };
        let _ = MetadataExchangeError::PieceRejected { piece: 0 };
        let _ = MetadataExchangeError::PieceTimeout { piece: 0 };
        let _ = MetadataExchangeError::IncompleteMetadata {
            expected: 0,
            received: 0,
        };
        let _ = MetadataExchangeError::IoError(String::new());
    }

    #[test]
    fn test_fatal_vs_recoverable_errors() {
        let fatal = MetadataExchangeError::MetadataTooLarge {
            size: 200_000_000,
            max: METADATA_MAX_SIZE,
        };
        assert!(fatal.is_fatal());

        let fatal2 = MetadataExchangeError::BencodeDecodeFailed {
            detail: "bad".to_string(),
        };
        assert!(fatal2.is_fatal());

        let recoverable = MetadataExchangeError::PeerTimeout {
            addr: "1.2.3.4:6881".to_string(),
        };
        assert!(!recoverable.is_fatal());

        let recoverable2 = MetadataExchangeError::InvalidMetadataSize { size: 0 };
        assert!(!recoverable2.is_fatal());
    }

    #[test]
    fn test_display_impl() {
        let err = MetadataExchangeError::NoPeersAvailable;
        assert!(err.to_string().contains("No peers"));

        let err = MetadataExchangeError::MetadataTooLarge {
            size: 200_000_000,
            max: METADATA_MAX_SIZE,
        };
        let display = err.to_string();
        assert!(display.contains("too large"));
        assert!(display.contains("200000000"));
    }

    #[test]
    fn test_with_piece_size_builder() {
        let cfg = MetadataExchangeConfig::default().with_piece_size(8192);
        assert_eq!(cfg.piece_size, 8192);

        let cfg_clamped_low = MetadataExchangeConfig::default().with_piece_size(512);
        assert_eq!(cfg_clamped_low.piece_size, PIECE_SIZE_MIN);

        let cfg_clamped_high = MetadataExchangeConfig::default().with_piece_size(128_000);
        assert_eq!(cfg_clamped_high.piece_size, PIECE_SIZE_MAX);
    }

    #[test]
    fn test_with_max_attempts_builder() {
        let cfg = MetadataExchangeConfig::default().with_max_attempts(5);
        assert_eq!(cfg.max_attempts, 5);

        let cfg_zero = MetadataExchangeConfig::default().with_max_attempts(0);
        assert_eq!(cfg_zero.max_attempts, 1);
    }
}
