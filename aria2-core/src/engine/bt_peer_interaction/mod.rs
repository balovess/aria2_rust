//! BT peer connection manager and initialization path.
//!
//! This module manages the interaction with BitTorrent peers, including:
//! - Connection establishment (plain and encrypted)
//! - Initial handshake and bitfield exchange
//! - Waiting for unchoke messages
//! - Peer connection lifecycle and readiness
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PeerInteractionCommand.h/.cc` — Peer connection lifecycle command
//! - `src/PeerConnection.cc/h` — Peer connection management
//! - `src/BtSetup.cc/h` — BT setup and initialization

mod types;

pub use types::{BtPeerConnectionOptions, BtPeerCryptoPolicy, PeerConnectionResult};

// ======================================================================
// BtPeerInteraction — peer connection lifecycle manager
// ======================================================================

use std::sync::Arc;
use std::time::Duration;

use aria2_protocol::bittorrent::message::types::BtMessage;
use futures::stream::{self, StreamExt};
use tokio::sync::Mutex;

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::network::OutboundNetworkPolicy;
use tracing::{debug, error, info, warn};

/// BT Peer Interaction Manager
///
/// Handles the lifecycle of peer connections from initial connection
/// through the handshake phase until they're ready for data transfer.
pub struct BtPeerInteraction;

const PEER_CONNECTION_DELAY_MS: u64 = crate::constants::BT_PEER_CONNECTION_DELAY_MS;
const PEER_MESSAGE_TIMEOUT_SECS: u64 = crate::constants::BT_PEER_MESSAGE_TIMEOUT_SECS;

impl BtPeerInteraction {
    /// Connect to multiple peers with automatic fallback strategies
    ///
    /// Attempts to connect to all provided peer addresses using:
    /// 1. MSE encryption if required or forced
    /// 2. Plain connection as fallback
    ///
    /// For each successful connection:
    /// - Sends initial unchoke and interested messages
    /// - Exchanges bitfields
    /// - Waits for unchoke from the peer
    ///
    /// # Arguments
    /// * `peer_addrs` - List of peer addresses to connect to
    /// * `info_hash_raw` - Torrent info hash for handshake
    /// * `num_pieces` - Total number of pieces (for bitfield size)
    /// * `require_crypto` - Whether to require encrypted connections
    /// * `force_encrypt` - Whether to force encryption (fallback to plain)
    ///
    /// # Returns
    /// * `PeerConnectionResult` containing connected peers and failure count
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_to_peers(
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        num_pieces: u32,
        piece_length: u32,
        total_length: u64,
        connection_options: &BtPeerConnectionOptions,
        utp_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
        policy: &OutboundNetworkPolicy,
    ) -> Result<PeerConnectionResult> {
        info!("[BT] Connecting to {} peers...", peer_addrs.len());

        // A peer can legitimately delay Unchoke while the tracker has already
        // supplied other usable peers. Establish each candidate independently
        // so one slow peer cannot prevent the piece scheduler from starting.
        let results = stream::iter(peer_addrs.iter().cloned())
            .map(|addr| {
                let utp_socket = utp_socket.clone();
                async move {
                    debug!("[BT] Connecting to peer {}:{}", addr.ip, addr.port);
                    let result = Self::connect_peer_ready(
                        &addr,
                        info_hash_raw,
                        connection_options,
                        num_pieces,
                        piece_length,
                        total_length,
                        utp_socket.clone(),
                        policy,
                    )
                    .await;
                    (addr, result)
                }
            })
            .buffer_unordered(peer_addrs.len().max(1))
            .collect::<Vec<_>>()
            .await;

        let mut active_connections = Vec::with_capacity(results.len());
        let mut failed_count = 0usize;
        for (addr, result) in results {
            match result {
                Ok(conn) => active_connections.push(conn),
                Err(e) => {
                    error!("[BT] Failed to connect peer {}: {}", addr.ip, e);
                    failed_count += 1;
                }
            }
        }

        info!("[BT] Active connections: {}", active_connections.len());

        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed".into(),
                },
            ));
        }

        Ok(PeerConnectionResult {
            connections: active_connections,
            failed_count,
        })
    }

    /// Establish and initialize one peer using the same crypto and protocol path
    /// as the initial peer batch.
    #[allow(clippy::too_many_arguments)]
    pub async fn connect_peer_ready(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_raw: &[u8; 20],
        connection_options: &BtPeerConnectionOptions,
        num_pieces: u32,
        piece_length: u32,
        total_length: u64,
        utp_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
        policy: &OutboundNetworkPolicy,
    ) -> Result<BtPeerConn> {
        let mut conn =
            Self::connect_single_peer(addr, info_hash_raw, connection_options, utp_socket, policy)
                .await?;
        conn.set_timeouts(
            connection_options.keep_alive_interval,
            connection_options.peer_timeout,
        );
        conn.sync_peer_identity();
        conn.allocate_session_resource(piece_length, num_pieces, total_length);
        info!(
            "[BT] Connected to peer {}:{} (encrypted={}, piece_length={}, total_length={})",
            addr.ip,
            addr.port,
            conn.is_encrypted(),
            piece_length,
            total_length
        );
        Self::initialize_connection(&mut conn, num_pieces, connection_options).await?;
        if let Err(error) = Self::wait_for_unchoke(&mut conn, addr).await {
            warn!("[BT] No unchoke from peer {}: {}", addr.ip, error);
        }
        Ok(conn)
    }

    /// Connect to a single peer with encryption fallback logic
    async fn connect_single_peer(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash_raw: &[u8; 20],
        connection_options: &BtPeerConnectionOptions,
        utp_socket: Option<Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>>,
        policy: &OutboundNetworkPolicy,
    ) -> Result<BtPeerConn> {
        if connection_options.enable_utp && !connection_options.crypto.require_mse {
            let endpoint = format!("{}:{}", addr.ip, addr.port)
                .parse::<std::net::SocketAddr>()
                .map_err(|error| {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "Invalid peer address '{}:{}': {error}",
                        addr.ip, addr.port
                    )))
                })?;
            let utp_result = BtPeerConn::connect_utp_with_policy(
                endpoint,
                info_hash_raw,
                connection_options.hybrid_info_hash_v2.as_ref(),
                crate::engine::bt_peer_connection::UtpConnectionOptions {
                    local_peer_id: connection_options.local_peer_id,
                    timeout: connection_options.connection_timeout,
                    listen_port: connection_options.utp_listen_port,
                    shared_socket: utp_socket,
                    dht_enabled: connection_options.dht_enabled,
                },
                policy,
            )
            .await;
            match utp_result {
                Ok(conn) => {
                    debug!("[BT] Connected to peer {}:{} over uTP", addr.ip, addr.port);
                    return Ok(conn);
                }
                Err(error) => {
                    debug!(
                        "[BT] uTP connection to {}:{} failed, trying TCP: {}",
                        addr.ip, addr.port, error
                    );
                }
            }
        }

        if connection_options.crypto.require_mse {
            // Try MSE encrypted connection
            BtPeerConn::connect_mse_with_policy(
                addr,
                info_hash_raw,
                connection_options.hybrid_info_hash_v2.as_ref(),
                crate::engine::bt_peer_connection::MseConnectionOptions {
                    force_encryption: connection_options.crypto.force_encryption,
                    prefer_encryption: connection_options.crypto.prefer_encryption,
                    local_peer_id: connection_options.local_peer_id,
                    timeout: connection_options.connection_timeout,
                    dht_enabled: connection_options.dht_enabled,
                },
                policy,
            )
            .await
        } else {
            // Try MSE first, fall back to plain
            let mse_result = BtPeerConn::connect_mse_with_policy(
                addr,
                info_hash_raw,
                connection_options.hybrid_info_hash_v2.as_ref(),
                crate::engine::bt_peer_connection::MseConnectionOptions {
                    force_encryption: connection_options.crypto.force_encryption,
                    prefer_encryption: connection_options.crypto.prefer_encryption,
                    local_peer_id: connection_options.local_peer_id,
                    timeout: connection_options.connection_timeout,
                    dht_enabled: connection_options.dht_enabled,
                },
                policy,
            )
            .await;
            match mse_result {
                Ok(conn) => Ok(conn),
                Err(_) => {
                    debug!("[BT] MSE failed, trying plain connection");
                    BtPeerConn::connect_plain_with_policy(
                        addr,
                        info_hash_raw,
                        connection_options.hybrid_info_hash_v2.as_ref(),
                        &connection_options.local_peer_id,
                        connection_options.connection_timeout,
                        connection_options.dht_enabled,
                        policy,
                    )
                    .await
                }
            }
        }
    }

    /// Initialize a newly established connection
    ///
    /// Sends initial protocol messages:
    /// - Unchoke (we allow them to request from us)
    /// - Interested (we want to download from them)
    /// - Bitfield (our current piece possession status)
    async fn initialize_connection(
        conn: &mut BtPeerConn,
        num_pieces: u32,
        connection_options: &BtPeerConnectionOptions,
    ) -> Result<()> {
        // Send initial messages
        conn.send_unchoke().await?;
        conn.send_interested().await?;

        // BEP 10 is part of the real connection setup. The peer-agent option
        // therefore travels on the wire before the piece loop starts.
        conn.send_extension_handshake_with_port(
            &connection_options.peer_agent,
            connection_options.listen_port,
        )
        .await?;

        // Send empty bitfield (we have nothing yet)
        let bf_len = (num_pieces as usize).div_ceil(8);
        let empty_bf = vec![0u8; bf_len];
        conn.send_bitfield(empty_bf).await?;

        // BEP 5 requires the Port message only when both sides advertised
        // DHT support. Private torrents clear `dht_enabled` before reaching
        // this path, so their info-hash is never announced through peers.
        if connection_options.dht_enabled
            && conn.remote_supports_dht()
            && let Some(port) = connection_options.listen_port
        {
            conn.send_port(port).await?;
        }

        // Small delay to allow processing
        tokio::time::sleep(Duration::from_millis(PEER_CONNECTION_DELAY_MS)).await;

        Ok(())
    }

    /// Apply peer state messages consumed while waiting for the initial
    /// unchoke. Peers commonly send their bitfield or HaveAll before Unchoke;
    /// dropping those messages leaves the piece selector with no availability.
    fn apply_setup_message(conn: &mut BtPeerConn, msg: BtMessage) -> bool {
        match msg {
            BtMessage::Have { piece_index } => {
                conn.update_peer_bitfield(piece_index as usize, 1);
                if conn
                    .session_resource
                    .as_ref()
                    .is_some_and(|resource| resource.is_seeder())
                {
                    conn.seeder = true;
                }
            }
            BtMessage::Bitfield { data } => {
                conn.set_peer_bitfield(&data);
                conn.seeder = conn
                    .session_resource
                    .as_ref()
                    .is_some_and(|resource| resource.is_seeder());
            }
            BtMessage::HaveAll => conn.mark_seeder(),
            BtMessage::HaveNone => {
                conn.set_peer_bitfield(&[]);
                conn.seeder = false;
            }
            BtMessage::Choke => conn.stats.peer_choking = true,
            BtMessage::Unchoke => {
                conn.stats.peer_choking = false;
                return true;
            }
            BtMessage::AllowedFast { index } => conn.add_peer_allowed_fast(index),
            _ => {}
        }
        false
    }

    /// Wait for an unchoke message from a peer
    ///
    /// Polls the connection for messages until we receive an Unchoke
    /// or hit the timeout/attempts limit.
    async fn wait_for_unchoke(
        conn: &mut BtPeerConn,
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
    ) -> Result<()> {
        debug!("[BT] Waiting for unchoke from {}:{}", addr.ip, addr.port);

        // Read only the initial setup burst here. A peer that delays Unchoke
        // must not hold the whole initial peer batch hostage; the piece loop
        // owns the connection after this point and can consume late setup
        // messages normally.
        let first_message = tokio::time::timeout(
            Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
            conn.read_message(),
        )
        .await;
        let mut got_unchoke = false;
        match first_message {
            Ok(Ok(Some(msg))) => {
                if Self::apply_setup_message(conn, msg) {
                    got_unchoke = true;
                    info!("[BT] Got unchoke from {}:{}", addr.ip, addr.port);
                }
                debug!("[BT] Applied message while waiting for unchoke");

                // Consume the rest of the setup burst without waiting for a
                // second full peer timeout. This preserves HaveAll/bitfield
                // state while still bounding slow peers to one initial wait.
                while let Ok(Ok(Some(msg))) =
                    tokio::time::timeout(Duration::from_millis(100), conn.read_message()).await
                {
                    if Self::apply_setup_message(conn, msg) {
                        got_unchoke = true;
                        info!("[BT] Got unchoke from {}:{}", addr.ip, addr.port);
                    }
                    debug!("[BT] Applied setup message while waiting for unchoke");
                }
            }
            Ok(Ok(None)) => {
                warn!("[BT] EOF from peer while waiting for unchoke");
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "Peer closed connection".into(),
                    },
                ));
            }
            Ok(Err(e)) => {
                error!("[BT] Error reading from peer: {}", e);
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("Read error: {}", e),
                    },
                ));
            }
            Err(_) => {
                debug!("[BT] Setup message wait elapsed; continuing with peer");
            }
        }

        if got_unchoke {
            return Ok(());
        }

        warn!(
            "[BT] Did not receive unchoke from {}:{} after {} attempts",
            addr.ip, addr.port, 1
        );
        Ok(()) // Continue anyway; the piece loop can receive it later.
    }

    /// Return the stable key used by the peer bitfield tracker.
    pub(crate) fn peer_tracker_key(conn: &BtPeerConn) -> String {
        conn.remote_peer_id()
            .map(|id| String::from_utf8_lossy(&id).into_owned())
            .unwrap_or_else(|| format!("{}:{}", conn.ip_addr, conn.port))
    }

    /// Initialize peer bitfield tracker for all connections
    ///
    /// Sets up tracking of which pieces each peer claims to have.
    ///
    /// # Arguments
    /// * `connections` - Slice of active peer connections
    /// * `num_pieces` - Total number of pieces in the torrent
    /// * `peer_tracker` - Mutable reference to the peer bitfield tracker
    pub(crate) fn initialize_peer_tracking(
        connections: &[BtPeerConn],
        _num_pieces: u32,
        peer_tracker: &mut crate::engine::bt_piece::PeerBitfieldTracker,
    ) {
        for conn in connections {
            let peer_key = Self::peer_tracker_key(conn);
            let bitfield = conn
                .session_resource
                .as_ref()
                .map_or(&[][..], |resource| resource.bitfield());
            peer_tracker.update_peer_bitfield(&peer_key, bitfield);
        }

        debug!(
            "[BT] Initialized peer tracking for {} peers",
            connections.len()
        );
    }
}
