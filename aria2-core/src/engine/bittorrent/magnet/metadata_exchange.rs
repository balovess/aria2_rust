//! Public BEP 9 metadata exchange interface backed by torrent peer actors.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::warn;

use aria2_protocol::bittorrent::message::extension::MAX_UT_METADATA_SIZE;

use crate::engine::bittorrent::dht::engine_set::DhtEngineSet;
use crate::engine::bittorrent::magnet::metadata_swarm::MetadataPeerSwarm;
use crate::network::OutboundNetworkPolicy;
use crate::request::request_group::{BtPeerSource, DownloadOptions};

pub(super) const METADATA_MAX_SIZE: u64 = MAX_UT_METADATA_SIZE as u64;
const PIECE_SIZE_MIN: u32 = 1024;
const PIECE_SIZE_MAX: u32 = 65536;
const DEFAULT_MAX_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, thiserror::Error)]
pub enum MetadataExchangeError {
    #[error("No peers available for metadata fetch")]
    NoPeersAvailable,
    #[error("All {attempts} peers failed, last error: {last_error}")]
    AllPeersFailed { attempts: usize, last_error: String },
    #[error("Connect to {addr} failed: {reason}")]
    PeerConnectFailed { addr: String, reason: String },
    #[error("Connect to {addr} timed out")]
    PeerTimeout { addr: String },
    #[error("Peer {addr} unsupported: {reason}")]
    UnsupportedPeer { addr: String, reason: String },
    #[error("Invalid metadata_size: {size}")]
    InvalidMetadataSize { size: u64 },
    #[error("Invalid metadata piece size: {size}")]
    InvalidPieceSize { size: u32 },
    #[error("metadata_size too large: {size} (max {max})")]
    MetadataTooLarge { size: u64, max: u64 },
    #[error("Invalid metadata piece {piece} length: {size} bytes (expected {expected})")]
    InvalidMetadataPiece {
        piece: u32,
        size: u64,
        expected: u64,
    },
    #[error("Bencode decode failed: {detail}")]
    BencodeDecodeFailed { detail: String },
    #[error("Piece {piece} rejected by peer")]
    PieceRejected { piece: u32 },
    #[error("ut_metadata timeout for piece {piece}")]
    PieceTimeout { piece: u32 },
    #[error("Incomplete metadata collection: expected {expected} bytes, received {received}")]
    IncompleteMetadata { expected: u64, received: u64 },
    #[error("IO error: {0}")]
    IoError(String),
}

impl MetadataExchangeError {
    pub fn addr(&self) -> Option<&str> {
        match self {
            Self::PeerConnectFailed { addr, .. }
            | Self::PeerTimeout { addr }
            | Self::UnsupportedPeer { addr, .. } => Some(addr),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
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
                "piece_size={size} is out of valid range [{PIECE_SIZE_MIN}-{PIECE_SIZE_MAX}], clamping"
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

/// Standalone public interface for fetching BEP 9 metadata from a peer list.
///
/// Connections are owned by the same peer actors used for payload transfer;
/// this standalone interface shuts its temporary swarm down after collection.
pub struct MetadataExchangeSession {
    config: MetadataExchangeConfig,
    outbound_network_policy: Arc<OutboundNetworkPolicy>,
}

impl MetadataExchangeSession {
    pub fn new(config: MetadataExchangeConfig) -> Self {
        Self {
            config,
            outbound_network_policy: Arc::new(OutboundNetworkPolicy::direct()),
        }
    }

    pub fn with_outbound_network_policy(mut self, policy: Arc<OutboundNetworkPolicy>) -> Self {
        self.outbound_network_policy = policy;
        self
    }

    pub async fn fetch_metadata(
        &self,
        info_hash: &[u8; 20],
        peers: &[SocketAddr],
    ) -> Result<Vec<u8>, MetadataExchangeError> {
        if self.config.piece_size == 0 {
            return Err(MetadataExchangeError::InvalidPieceSize {
                size: self.config.piece_size,
            });
        }
        if peers.is_empty() {
            return Err(MetadataExchangeError::NoPeersAvailable);
        }

        let mut bootstrap = MetadataPeerSwarm::new(self.config);
        let peer_sources = peers
            .iter()
            .take(self.config.max_peers_to_try)
            .copied()
            .map(|endpoint| (endpoint, BtPeerSource::Unknown))
            .collect::<Vec<_>>();
        let options = DownloadOptions::default();
        let peer_id = aria2_protocol::bittorrent::peer::id::generate_peer_id();
        let result = async {
            bootstrap
                .add_peers(
                    &peer_sources,
                    info_hash,
                    peer_id,
                    &options,
                    &DhtEngineSet::default(),
                    Arc::clone(&self.outbound_network_policy),
                )
                .await?;
            bootstrap.fetch_metadata().await
        }
        .await;
        bootstrap.shutdown().await;
        result
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
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(session.fetch_metadata(&target_hash, &[]));

        assert!(matches!(
            result,
            Err(MetadataExchangeError::NoPeersAvailable)
        ));
    }

    #[test]
    fn test_extension_handshake_uses_complete_bep10_frame() {
        use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
        use aria2_protocol::bittorrent::message::serializer::serialize_extended;

        let frame = serialize_extended(0, ExtensionHandshake::new().to_bytes());
        let frame_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;

        assert_eq!(frame_len, frame.len() - 4);
        assert_eq!(&frame[4..6], &[20, 0]);
        let parsed = ExtensionHandshake::from_bytes(&frame[6..]).unwrap();
        assert_eq!(parsed.ut_metadata_id(), Some(1));
        assert_eq!(parsed.metadata_size(), None);
    }

    #[test]
    fn test_ut_metadata_request_uses_negotiated_extension_id() {
        use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;
        use aria2_protocol::bittorrent::message::serializer::serialize_extended;

        let frame = serialize_extended(7, UtMetadataMessage::Request { piece: 3 }.to_payload());
        let frame_len = u32::from_be_bytes(frame[..4].try_into().unwrap()) as usize;

        assert_eq!(frame_len, frame.len() - 4);
        assert_eq!(&frame[4..6], &[20, 7]);
        assert_eq!(
            UtMetadataMessage::from_payload(&frame[6..]).unwrap(),
            UtMetadataMessage::Request { piece: 3 }
        );
    }

    #[test]
    fn test_display_impl() {
        assert!(
            MetadataExchangeError::NoPeersAvailable
                .to_string()
                .contains("No peers")
        );
        let too_large = MetadataExchangeError::MetadataTooLarge {
            size: 200_000_000,
            max: METADATA_MAX_SIZE,
        };
        let display = too_large.to_string();
        assert!(display.contains("too large"));
        assert!(display.contains("200000000"));
    }

    #[test]
    fn test_with_piece_size_builder() {
        assert_eq!(
            MetadataExchangeConfig::default()
                .with_piece_size(8192)
                .piece_size,
            8192
        );
        assert_eq!(
            MetadataExchangeConfig::default()
                .with_piece_size(512)
                .piece_size,
            PIECE_SIZE_MIN
        );
        assert_eq!(
            MetadataExchangeConfig::default()
                .with_piece_size(128_000)
                .piece_size,
            PIECE_SIZE_MAX
        );
    }

    #[test]
    fn test_with_max_attempts_builder() {
        assert_eq!(
            MetadataExchangeConfig::default()
                .with_max_attempts(5)
                .max_attempts,
            5
        );
        assert_eq!(
            MetadataExchangeConfig::default()
                .with_max_attempts(0)
                .max_attempts,
            1
        );
    }

    #[test]
    fn test_fetch_metadata_rejects_zero_piece_size() {
        let session = MetadataExchangeSession::new(MetadataExchangeConfig {
            piece_size: 0,
            ..MetadataExchangeConfig::default()
        });
        let target_hash = [0u8; 20];
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(session.fetch_metadata(&target_hash, &["127.0.0.1:1".parse().unwrap()]));

        assert!(matches!(
            result,
            Err(MetadataExchangeError::InvalidPieceSize { size: 0 })
        ));
    }
}
