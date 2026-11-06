//! Types shared by the BitTorrent peer connection manager.

use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::request::request_group::DownloadOptions;
use std::time::Duration;

/// Outbound BitTorrent crypto policy resolved from the original option names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BtPeerCryptoPolicy {
    /// Do not fall back to the legacy unencrypted handshake.
    pub require_mse: bool,
    /// Require RC4 after MSE negotiation.
    pub force_encryption: bool,
    /// Prefer RC4 when the peer offers both MSE methods.
    pub prefer_encryption: bool,
}

/// Task-scoped values consumed by the outbound peer connection path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtPeerConnectionOptions {
    pub crypto: BtPeerCryptoPolicy,
    pub connection_timeout: Duration,
    pub keep_alive_interval: Duration,
    pub peer_timeout: Duration,
    pub local_peer_id: [u8; 20],
    pub peer_agent: String,
    pub enable_utp: bool,
    pub utp_listen_port: Option<u16>,
    /// Advertise BEP 5 support on outbound peer handshakes.
    pub dht_enabled: bool,
    /// TCP port advertised in the BEP 10 `p` key.
    pub listen_port: Option<u16>,
    /// v2 info-hash for a hybrid BEP 52 torrent.
    pub hybrid_info_hash_v2: Option<[u8; 32]>,
}

impl BtPeerConnectionOptions {
    pub fn from_download_options(options: &DownloadOptions, local_peer_id: [u8; 20]) -> Self {
        Self {
            crypto: BtPeerCryptoPolicy {
                require_mse: options.bt_require_crypto || options.bt_force_encrypt,
                force_encryption: options.bt_force_encrypt,
                prefer_encryption: options.bt_min_crypto_level.eq_ignore_ascii_case("arc4")
                    || options.bt_force_encrypt,
            },
            connection_timeout: Duration::from_secs(options.peer_connection_timeout),
            keep_alive_interval: Duration::from_secs(options.bt_keep_alive_interval),
            peer_timeout: Duration::from_secs(options.bt_timeout),
            local_peer_id,
            peer_agent: options.peer_agent.clone(),
            enable_utp: options.enable_utp,
            utp_listen_port: options.utp_listen_port,
            dht_enabled: options.enable_dht || options.enable_dht6,
            listen_port: None,
            hybrid_info_hash_v2: None,
        }
    }
}

/// Result of a batch peer connection attempt.
pub struct PeerConnectionResult {
    /// Successfully connected peers.
    pub connections: Vec<BtPeerConn>,
    /// Number of failed connections.
    pub failed_count: usize,
}
