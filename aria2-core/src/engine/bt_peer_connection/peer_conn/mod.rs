//! Main BitTorrent peer connection struct.
//!
//! [`BtPeerConn`] composes an inner connection (plain/encrypted/uTP),
//! a send buffer, session resource, keep-alive management, and peer statistics.
//!
//! This module is split into focused sub-modules:
//! - [`connect`] — connection constructors (MSE, plain, uTP, stub)
//! - [`session`] — session resource lifecycle, bitfield, fast extension, AllowedFast
//! - [`keepalive`] — keep-alive timing, send buffering, PEX, bookkeeping
//! - [`messages`] — protocol message senders, message reading, write helpers

mod connect;
pub use connect::{MseConnectionOptions, UtpConnectionOptions};
mod keepalive;
mod messages;
mod session;

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::engine::peer_stats::PeerStats;
use crate::request::request_group::BtPeerSource;

use super::session_resource::PeerSessionResource;
use super::types::{ConnectionType, SendBuffer};
use super::utp_connection::UtpPeerConnection;

// ---------------------------------------------------------------------------
// Keep-alive / timeout constants
// ---------------------------------------------------------------------------

/// Keep-alive interval (2 minutes, per BitTorrent spec).
pub(super) const KEEPALIVE_INTERVAL_SECS: u64 = 120;

/// Timeout for peer inactivity before considering the connection dead.
pub(super) const PEER_TIMEOUT_SECS: u64 = 180;

// ---------------------------------------------------------------------------
// InnerConnection — plain / encrypted / uTP
// ---------------------------------------------------------------------------

#[allow(clippy::large_enum_variant)]
pub(crate) enum InnerConnection {
    Plain(aria2_protocol::bittorrent::peer::connection::PeerConnection),
    Encrypted(aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection),
    Utp(UtpPeerConnection),
}

/// Post-handshake messages sent by the owning peer actor exactly once.
pub(crate) struct PeerActorStartup {
    pub(crate) peer_agent: String,
    pub(crate) listen_port: Option<u16>,
    pub(crate) allowed_fast: Vec<u32>,
}

/// Identity that remains attached to one peer connection across worker restarts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PeerActorId(pub(crate) u64);

impl PeerActorId {
    pub(crate) fn allocate() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }
}

// ---------------------------------------------------------------------------
// BtPeerConn
// ---------------------------------------------------------------------------

/// Peer connection abstraction that supports both plain and encrypted (MSE)
/// connections as well as uTP.
///
/// This mirrors the original aria2 C++ architecture where connection management
/// is separated from the download command logic (see BtRuntime in original).
///
/// Composes:
/// - An [`InnerConnection`] for actual I/O.
/// - A [`SendBuffer`] for batching outbound messages.
/// - An optional [`PeerSessionResource`] for per-session state.
/// - Keep-alive / timeout tracking.
/// - [`PeerStats`] for integration with the choking algorithm.
pub struct BtPeerConn {
    /// Stable identity for the I/O actor associated with this connection.
    pub(crate) actor_id: PeerActorId,
    pub(crate) inner: InnerConnection,

    // -----------------------------------------------------------------------
    // Peer identity
    // -----------------------------------------------------------------------
    /// Remote IP address.
    pub(crate) ip_addr: String,
    /// Remote port.
    pub(crate) port: u16,
    /// 20-byte peer ID (set after handshake).
    pub(crate) peer_id: Option<[u8; 20]>,
    /// Client name learned from the remote BEP 10 extension handshake.
    pub(crate) remote_client: Arc<RwLock<Option<String>>>,
    /// Whether this was an incoming (accepted) connection.
    pub(crate) incoming: bool,
    /// Discovery mechanism that supplied this peer address.
    pub(crate) source: BtPeerSource,
    /// Whether this is a local network peer.
    pub(crate) local_peer: bool,
    /// Whether the peer disconnected gracefully.
    pub(crate) disconnected_gracefully: bool,
    /// Whether this peer is a seeder (has all pieces).
    pub(crate) seeder: bool,

    // -----------------------------------------------------------------------
    // Timing
    // -----------------------------------------------------------------------
    /// First contact time.
    pub(crate) first_contact_time: Instant,

    // -----------------------------------------------------------------------
    // Connection classification
    // -----------------------------------------------------------------------
    /// Connection type (TCP or uTP).
    pub(crate) connection_type: ConnectionType,
    /// Pieces the remote peer allowed us to request while it is choking us.
    pub(crate) peer_allowed_fast: HashSet<u32>,
    /// Pieces we allowed the remote peer to request while we are choking it.
    pub(crate) am_allowed_fast: HashSet<u32>,

    // -----------------------------------------------------------------------
    // Session resource (allocated when active)
    // -----------------------------------------------------------------------
    /// Per-session resource. `Some` while the peer is active, `None` when
    /// disconnected or not yet fully initialised.
    pub(crate) session_resource: Option<PeerSessionResource>,

    // -----------------------------------------------------------------------
    // Send buffering (C++ SocketBuffer)
    // -----------------------------------------------------------------------
    /// Send buffer for batching outgoing messages.
    pub(crate) send_buffer: SendBuffer,

    // -----------------------------------------------------------------------
    // Keep-alive / timeout tracking
    // -----------------------------------------------------------------------
    /// Last time we sent a keep-alive (or any message).
    pub(crate) last_keepalive_sent: Instant,
    /// Last time we received any message from the peer.
    pub(crate) last_message_received: Instant,
    /// Configured interval for sending keep-alive frames.
    pub(crate) keep_alive_interval: Duration,
    /// Configured maximum interval without receiving a peer message.
    pub(crate) peer_timeout: Duration,

    // -----------------------------------------------------------------------
    // Statistics (integration with choking algorithm)
    // -----------------------------------------------------------------------
    /// Associated peer statistics.
    pub(crate) stats: PeerStats,

    // -----------------------------------------------------------------------
    // PEX (BEP 11) — inbound peer accumulation
    // -----------------------------------------------------------------------
    /// Peers discovered via incoming PEX messages while reading blocks.
    /// The download loop drains this after each iteration to add new peers
    /// to the connection pool without threading extension-update types
    /// through the block-message handler.
    pub(crate) pending_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Whether this connection may receive and accumulate BEP 11 peers.
    pub(crate) pex_enabled: bool,

    /// Upload state for pieces already verified during an active download.
    /// This is intentionally kept on the duplex peer connection so upload
    /// requests can be served while the same socket is downloading blocks.
    pub(crate) upload_state: Option<crate::engine::bt_upload_session::BtUploadState>,
    pub(crate) upload_progress:
        Option<std::sync::Arc<crate::request::request_group::AtomicProgress>>,
    pub(crate) actor_startup: Option<PeerActorStartup>,
}

impl BtPeerConn {
    /// Apply protocol state carried by a peer message.
    ///
    /// Both actor-driven and coordinator-driven reads use this operation so
    /// availability and choke state have one transition implementation.
    pub(crate) fn apply_peer_state_message(
        &mut self,
        message: &aria2_protocol::bittorrent::message::types::BtMessage,
    ) {
        use aria2_protocol::bittorrent::message::types::BtMessage;

        match message {
            BtMessage::AllowedFast { index } => self.add_peer_allowed_fast(*index),
            BtMessage::Have { piece_index } => {
                self.update_peer_bitfield(*piece_index as usize, 1);
            }
            BtMessage::Bitfield { data } => self.set_peer_bitfield(data),
            BtMessage::HaveAll => self.mark_seeder(),
            BtMessage::HaveNone => {
                self.seeder = false;
                self.set_peer_bitfield(&[]);
            }
            BtMessage::Choke => self.stats.peer_choking = true,
            BtMessage::Unchoke => self.stats.peer_choking = false,
            _ => {}
        }
    }

    pub(crate) fn configure_upload_with_auto_unchoke(
        &mut self,
        config: &crate::engine::bt_upload_session::BtSeedingConfig,
        upload_limiter: crate::rate_limiter::RateLimiter,
        num_pieces: u32,
        piece_length: u32,
        auto_unchoke: bool,
    ) {
        let mut state = crate::engine::bt_upload_session::BtUploadState::new_with_limiter(
            config,
            upload_limiter,
        );
        state.configure_message_validator(num_pieces, piece_length);
        state.set_auto_unchoke(auto_unchoke);
        self.upload_state = Some(state);
    }

    pub(crate) async fn handle_upload_message(
        &mut self,
        message: aria2_protocol::bittorrent::message::types::BtMessage,
        provider: &dyn crate::engine::bt_upload_session::PieceDataProvider,
    ) -> crate::error::Result<u64> {
        let Some(mut state) = self.upload_state.take() else {
            return Ok(0);
        };
        let message_for_stats = message.clone();
        let result = state.handle_message(self, message, provider).await;
        self.upload_state = Some(state);
        if let Ok(bytes) = result {
            match message_for_stats {
                aria2_protocol::bittorrent::message::types::BtMessage::Interested => {
                    self.stats.peer_interested = true;
                }
                aria2_protocol::bittorrent::message::types::BtMessage::NotInterested => {
                    self.stats.peer_interested = false;
                }
                aria2_protocol::bittorrent::message::types::BtMessage::Choke => {
                    self.stats.peer_choking = true;
                }
                aria2_protocol::bittorrent::message::types::BtMessage::Unchoke => {
                    self.stats.peer_choking = false;
                }
                _ => {}
            }
            self.record_uploaded_bytes(bytes);
        }
        result
    }

    pub(crate) fn has_pending_upload_messages(&self) -> bool {
        self.upload_state
            .as_ref()
            .is_some_and(|state| state.has_pending_messages())
    }

    pub(crate) fn discard_pending_upload_messages(&mut self) {
        if let Some(state) = self.upload_state.as_mut() {
            state.discard_pending_messages();
            self.stats.outstanding_upload_count = state.outstanding_upload_count();
        }
    }

    pub(crate) async fn flush_upload_messages(
        &mut self,
        provider: &dyn crate::engine::bt_upload_session::PieceDataProvider,
    ) -> crate::error::Result<u64> {
        let Some(mut state) = self.upload_state.take() else {
            return Ok(0);
        };
        let result = state.flush_pending_messages(self, provider).await;
        self.upload_state = Some(state);
        if let Ok(bytes) = result {
            self.record_uploaded_bytes(bytes);
        }
        result
    }

    fn record_uploaded_bytes(&mut self, bytes: u64) {
        self.stats.am_choking = self
            .upload_state
            .as_ref()
            .is_some_and(|upload| upload.is_peer_choked());
        self.stats.outstanding_upload_count = self.upload_state.as_ref().map_or(
            0,
            crate::engine::bt_upload_session::BtUploadState::outstanding_upload_count,
        );
        self.stats.on_data_sent(bytes);
        if bytes > 0
            && let Some(progress) = self.upload_progress.as_ref()
        {
            progress.add_upload_length(bytes);
            progress.set_upload_speed(self.stats.upload_speed.max(0.0) as u64);
        }
    }

    pub(crate) async fn choke_upload_peer(&mut self) -> crate::error::Result<()> {
        let Some(mut state) = self.upload_state.take() else {
            return Ok(());
        };
        let result = state.choke_peer(self).await;
        self.upload_state = Some(state);
        if result.is_ok() {
            self.stats.am_choking = true;
        }
        result
    }

    pub(crate) async fn unchoke_upload_peer(&mut self) -> crate::error::Result<()> {
        let Some(mut state) = self.upload_state.take() else {
            return Ok(());
        };
        let result = state.unchoke_peer(self).await;
        self.upload_state = Some(state);
        if result.is_ok() {
            self.stats.am_choking = false;
        }
        result
    }

    pub(crate) async fn announce_upload_availability(
        &mut self,
        provider: &dyn crate::engine::bt_upload_session::PieceDataProvider,
    ) -> crate::error::Result<()> {
        let Some(mut state) = self.upload_state.take() else {
            return Ok(());
        };
        let result = state.send_piece_availability(self, provider).await;
        self.upload_state = Some(state);
        result
    }

    pub(crate) fn set_upload_counter(
        &mut self,
        counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
    ) {
        if let Some(state) = self.upload_state.as_mut() {
            state.set_upload_counter(counter);
        }
    }

    pub(crate) fn set_upload_progress(
        &mut self,
        progress: std::sync::Arc<crate::request::request_group::AtomicProgress>,
    ) {
        self.upload_progress = Some(progress);
    }
}

impl BtPeerConn {
    /// Return the peer address supplied by discovery or the incoming socket.
    pub fn remote_ip(&self) -> &str {
        &self.ip_addr
    }

    /// Return the peer's advertised or discovered port.
    pub fn remote_port(&self) -> u16 {
        self.port
    }

    /// Return the peer ID learned during the BitTorrent handshake, if any.
    pub fn peer_id(&self) -> Option<[u8; 20]> {
        self.peer_id
    }

    /// Whether this connection was accepted by the local peer listener.
    pub fn is_incoming(&self) -> bool {
        self.incoming
    }

    /// Whether this is a local-network peer.
    pub fn is_local_peer(&self) -> bool {
        self.local_peer
    }

    /// Whether the peer sent a graceful disconnect indication.
    pub fn disconnected_gracefully(&self) -> bool {
        self.disconnected_gracefully
    }

    /// Whether the peer has advertised all torrent pieces.
    pub fn is_seeder(&self) -> bool {
        self.seeder
    }

    /// Return the connection's first-contact timestamp.
    pub fn first_contact_time(&self) -> Instant {
        self.first_contact_time
    }

    /// Return read-only transfer and choking statistics for this peer.
    pub fn stats(&self) -> &PeerStats {
        &self.stats
    }

    /// Return the discovery source that supplied this peer.
    pub fn source(&self) -> BtPeerSource {
        self.source
    }

    pub(crate) fn set_source(&mut self, source: BtPeerSource) {
        self.source = source;
    }

    pub(crate) fn set_pex_enabled(&mut self, enabled: bool) {
        self.pex_enabled = enabled;
        if !enabled {
            self.pending_pex_peers.clear();
        }
    }

    pub(crate) fn is_pex_enabled(&self) -> bool {
        self.pex_enabled
    }

    /// Returns the remote peer ID learned during the protocol handshake.
    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        match &self.inner {
            InnerConnection::Plain(conn) => conn.remote_peer_id().copied(),
            InnerConnection::Encrypted(conn) => conn.remote_peer_id().copied(),
            InnerConnection::Utp(conn) => conn.remote_peer_id(),
        }
    }

    /// Whether the remote BitTorrent handshake advertised BEP 5 DHT support.
    pub fn remote_supports_dht(&self) -> bool {
        match &self.inner {
            InnerConnection::Plain(conn) => conn.remote_supports_dht(),
            InnerConnection::Encrypted(conn) => conn.remote_supports_dht(),
            InnerConnection::Utp(conn) => conn.remote_supports_dht(),
        }
    }

    /// Whether the remote BitTorrent handshake advertised BEP 6 support.
    pub fn remote_supports_fast_extension(&self) -> bool {
        match &self.inner {
            InnerConnection::Plain(conn) => conn.remote_supports_fast_extension(),
            InnerConnection::Encrypted(conn) => conn.remote_supports_fast_extension(),
            InnerConnection::Utp(conn) => conn.remote_supports_fast_extension(),
        }
    }

    /// Synchronize the peer identity captured by the transport handshake.
    pub(crate) fn sync_peer_identity(&mut self) {
        if let Some(peer_id) = self.remote_peer_id() {
            self.peer_id = Some(peer_id);
            self.stats.peer_id = peer_id;
        }
    }

    pub fn remote_endpoint(&self) -> Option<std::net::SocketAddr> {
        match &self.inner {
            InnerConnection::Utp(conn) => conn.remote_addr(),
            InnerConnection::Plain(_) | InnerConnection::Encrypted(_) => self
                .ip_addr
                .parse::<std::net::IpAddr>()
                .ok()
                .map(|ip| std::net::SocketAddr::new(ip, self.port)),
        }
    }
}
