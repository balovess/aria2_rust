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
mod keepalive;
mod messages;
mod session;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::engine::peer_stats::PeerStats;

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
    pub(crate) inner: InnerConnection,

    // -----------------------------------------------------------------------
    // Peer identity
    // -----------------------------------------------------------------------
    /// Remote IP address.
    pub ip_addr: String,
    /// Remote port.
    pub port: u16,
    /// 20-byte peer ID (set after handshake).
    pub peer_id: Option<[u8; 20]>,
    /// Whether this was an incoming (accepted) connection.
    pub incoming: bool,
    /// Whether this is a local network peer.
    pub local_peer: bool,
    /// Whether the peer disconnected gracefully.
    pub disconnected_gracefully: bool,
    /// Whether this peer is a seeder (has all pieces).
    pub seeder: bool,

    // -----------------------------------------------------------------------
    // Timing
    // -----------------------------------------------------------------------
    /// First contact time.
    pub first_contact_time: Instant,

    // -----------------------------------------------------------------------
    // Connection classification
    // -----------------------------------------------------------------------
    /// Connection type (TCP or uTP).
    pub connection_type: ConnectionType,
    /// Set of piece indices for which the peer has sent an AllowedFast message.
    /// Pieces in this set can be requested even when the peer is choked.
    pub allowed_fast: HashSet<u32>,

    // -----------------------------------------------------------------------
    // Session resource (allocated when active)
    // -----------------------------------------------------------------------
    /// Per-session resource. `Some` while the peer is active, `None` when
    /// disconnected or not yet fully initialised.
    pub session_resource: Option<PeerSessionResource>,

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
    pub stats: PeerStats,

    // -----------------------------------------------------------------------
    // PEX (BEP 11) — inbound peer accumulation
    // -----------------------------------------------------------------------
    /// Peers discovered via incoming PEX messages while reading blocks.
    /// The download loop drains this after each iteration to add new peers
    /// to the connection pool. This avoids having to thread extension-update
    /// types through the legacy `BtMessageHandler` API.
    pub pending_pex_peers: Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr>,
    /// Whether this connection may receive and accumulate BEP 11 peers.
    pub(crate) pex_enabled: bool,
}

impl BtPeerConn {
    pub(crate) fn set_pex_enabled(&mut self, enabled: bool) {
        self.pex_enabled = enabled;
        if !enabled {
            self.pending_pex_peers.clear();
        }
    }

    pub(crate) fn is_pex_enabled(&self) -> bool {
        self.pex_enabled
    }

    /// Transfer a TCP connection into the upload-session transport seam.
    ///
    /// uTP remains a download transport for now; the previous seeding path
    /// already excluded it, while plain and MSE connections can both serve
    /// upload messages without losing their transport state.
    pub(crate) fn into_upload_connection(
        self,
    ) -> Option<crate::engine::bt_upload_session::BtUploadConnection> {
        match self.inner {
            InnerConnection::Plain(connection) => Some(
                crate::engine::bt_upload_session::BtUploadConnection::Plain(Box::new(connection)),
            ),
            InnerConnection::Encrypted(connection) => Some(
                crate::engine::bt_upload_session::BtUploadConnection::Encrypted(Box::new(
                    connection,
                )),
            ),
            InnerConnection::Utp(_) => None,
        }
    }

    /// Returns the remote peer ID learned during the protocol handshake.
    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        match &self.inner {
            InnerConnection::Plain(conn) => conn.remote_peer_id,
            InnerConnection::Encrypted(conn) => conn.remote_peer_id().copied(),
            InnerConnection::Utp(conn) => conn.remote_peer_id(),
        }
    }

    /// Synchronize the peer identity captured by the transport handshake.
    pub fn sync_peer_identity(&mut self) {
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
