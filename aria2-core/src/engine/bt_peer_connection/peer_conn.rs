//! Main BitTorrent peer connection struct.
//!
//! [`BtPeerConn`] composes an inner connection (plain/encrypted/uTP),
//! a send buffer, session resource, keep-alive management, and peer statistics.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::engine::peer_stats::PeerStats;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

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
    send_buffer: SendBuffer,

    // -----------------------------------------------------------------------
    // Keep-alive / timeout tracking
    // -----------------------------------------------------------------------
    /// Last time we sent a keep-alive (or any message).
    last_keepalive_sent: Instant,
    /// Last time we received any message from the peer.
    last_message_received: Instant,

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
}

impl BtPeerConn {
    // -----------------------------------------------------------------------
    // Connection constructors
    // -----------------------------------------------------------------------

    /// Connect via MSE (Message Stream Encryption) over TCP.
    pub async fn connect_mse(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(addr, info_hash, require_encryption).await {
            Ok(conn) => {
                let now = Instant::now();
                Ok(Self {
                    inner: InnerConnection::Encrypted(conn),
                    ip_addr: addr.ip.clone(),
                    port: addr.port,
                    peer_id: None,
                    incoming: false,
                    local_peer: false,
                    disconnected_gracefully: false,
                    seeder: false,
                    first_contact_time: now,
                    connection_type: ConnectionType::Tcp,
                    allowed_fast: HashSet::new(),
                    session_resource: None,
                    send_buffer: SendBuffer::new(),
                    last_keepalive_sent: now,
                    last_message_received: now,
                    stats: PeerStats::new([0u8; 20], std::net::SocketAddr::new(
                        addr.ip.parse().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                        addr.port,
                    )),
                    pending_pex_peers: Vec::new(),
                })
            }
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Connect via plain TCP.
    pub async fn connect_plain(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, info_hash)
            .await
        {
            Ok(conn) => {
                let now = Instant::now();
                Ok(Self {
                    inner: InnerConnection::Plain(conn),
                    ip_addr: addr.ip.clone(),
                    port: addr.port,
                    peer_id: None,
                    incoming: false,
                    local_peer: false,
                    disconnected_gracefully: false,
                    seeder: false,
                    first_contact_time: now,
                    connection_type: ConnectionType::Tcp,
                    allowed_fast: HashSet::new(),
                    session_resource: None,
                    send_buffer: SendBuffer::new(),
                    last_keepalive_sent: now,
                    last_message_received: now,
                    stats: PeerStats::new(
                        [0u8; 20],
                        std::net::SocketAddr::new(
                            addr.ip
                                .parse()
                                .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                            addr.port,
                        ),
                    ),
                    pending_pex_peers: Vec::new(),
                })
            }
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Create a stub connection for unit testing.
    ///
    /// This creates a loopback TCP connection pair. The returned
    /// `BtPeerConn` is not actually connected to any real peer,
    /// but has enough structure for unit tests that need to inspect
    /// or modify fields like `session_resource`, `allowed_fast`, etc.
    #[cfg(test)]
    pub fn new_stub(info_hash: &[u8; 20]) -> Self {
        let now = Instant::now();
        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

        // Create a loopback connection pair. We only need one side
        // for the stub; the other is dropped.
        let rt = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        let stream = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let local_addr = listener.local_addr().unwrap();
            let (_, stream) = tokio::join!(
                tokio::net::TcpStream::connect(local_addr),
                listener.accept()
            );
            stream.unwrap().0
        });

        let peer_conn =
            aria2_protocol::bittorrent::peer::connection::PeerConnection::from_stream_with_peer(
                stream, [0u8; 20],
            );

        Self {
            inner: InnerConnection::Plain(peer_conn),
            ip_addr: "127.0.0.1".to_string(),
            port: 0,
            peer_id: Some(*info_hash),
            incoming: false,
            local_peer: true,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Tcp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            stats: PeerStats::new([0u8; 20], addr),
            pending_pex_peers: Vec::new(),
        }
    }

    /// Connect via uTP (Micro Transport Protocol).
    ///
    /// uTP is a UDP-based transport protocol that provides:
    /// - Reliable, ordered delivery
    /// - LEDBAT congestion control (low priority, background traffic)
    /// - Better performance on congested networks
    /// - NAT traversal benefits
    pub async fn connect_utp(addr: std::net::SocketAddr, info_hash: &[u8; 20]) -> Result<Self> {
        let utp_conn = UtpPeerConnection::connect(addr, info_hash).await?;
        let now = Instant::now();

        Ok(Self {
            inner: InnerConnection::Utp(utp_conn),
            ip_addr: addr.ip().to_string(),
            port: addr.port(),
            peer_id: None,
            incoming: false,
            local_peer: false,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Utp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            stats: PeerStats::new([0u8; 20], addr),
            pending_pex_peers: Vec::new(),
        })
    }

    /// Create a uTP connection from an existing socket.
    ///
    /// Used when accepting incoming uTP connections.
    pub fn from_utp_socket(
        socket: Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>,
        conn_id: u16,
        info_hash: &[u8; 20],
    ) -> Self {
        let utp_conn = UtpPeerConnection::new(socket, conn_id, *info_hash);
        let now = Instant::now();

        Self {
            inner: InnerConnection::Utp(utp_conn),
            ip_addr: String::new(),
            port: 0,
            peer_id: None,
            incoming: true,
            local_peer: false,
            disconnected_gracefully: false,
            seeder: false,
            first_contact_time: now,
            connection_type: ConnectionType::Utp,
            allowed_fast: HashSet::new(),
            session_resource: None,
            send_buffer: SendBuffer::new(),
            last_keepalive_sent: now,
            last_message_received: now,
            stats: PeerStats::new(
                [0u8; 20],
                std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED), 0),
            ),
            pending_pex_peers: Vec::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Connection classification
    // -----------------------------------------------------------------------

    /// Get the connection type.
    pub fn connection_type(&self) -> ConnectionType {
        self.connection_type
    }

    /// Check if this is a uTP connection.
    pub fn is_utp(&self) -> bool {
        self.connection_type == ConnectionType::Utp
    }

    // -----------------------------------------------------------------------
    // AllowedFast (BEP 6)
    // -----------------------------------------------------------------------

    /// Add a piece index to the AllowedFast set.
    ///
    /// Called when an AllowedFast message is received from this peer.
    /// Pieces in the allowed_fast set can be requested even when the peer
    /// is choked (BEP 6 / Fast Extension).
    pub fn add_allowed_fast(&mut self, index: u32) {
        self.allowed_fast.insert(index);
    }

    /// Check whether a piece index is in the AllowedFast set.
    ///
    /// Returns true if the peer has granted fast access to this piece,
    /// meaning a Request can be sent even while the peer is choked.
    pub fn is_allowed_fast(&self, index: u32) -> bool {
        self.allowed_fast.contains(&index)
    }

    /// Get a reference to the full AllowedFast set.
    ///
    /// Returns all piece indices that this peer has allowed us to request
    /// via BEP 6 Fast Extension, even when choked.
    pub fn allowed_fast_set(&self) -> &HashSet<u32> {
        &self.allowed_fast
    }

    // -----------------------------------------------------------------------
    // Fast Extension delegation
    // -----------------------------------------------------------------------

    /// Check whether fast extension is enabled for this connection.
    ///
    /// Delegates to the session resource's `is_fast_extension_enabled()`.
    /// Returns `false` if no session resource is allocated.
    pub fn is_fast_extension_enabled(&self) -> bool {
        self.session_resource
            .as_ref()
            .map_or(false, |sr| sr.is_fast_extension_enabled())
    }

    /// Enable or disable fast extension for this connection.
    ///
    /// Delegates to the session resource's `set_fast_extension_enabled()`.
    /// Does nothing if no session resource is allocated.
    pub fn set_fast_extension_enabled(&mut self, enabled: bool) {
        if let Some(sr) = &mut self.session_resource {
            sr.set_fast_extension_enabled(enabled);
        }
    }

    // -----------------------------------------------------------------------
    // Session resource lifecycle
    // -----------------------------------------------------------------------

    /// Allocate a [`PeerSessionResource`] for this connection.
    ///
    /// Called when the peer becomes active (after successful handshake).
    /// Does nothing if a session resource is already allocated.
    pub fn allocate_session_resource(&mut self, piece_length: u32, total_length: u64) {
        if self.session_resource.is_none() {
            self.session_resource = Some(PeerSessionResource::new(piece_length, total_length));
        }
    }

    /// Release the [`PeerSessionResource`], dropping all per-session state.
    pub fn release_session_resource(&mut self) {
        self.session_resource = None;
    }

    /// Reconfigure the session resource for new torrent parameters.
    ///
    /// No-op if no session resource is allocated.
    pub fn reconfigure_session_resource(&mut self, piece_length: u32, total_length: u64) {
        if let Some(ref mut res) = self.session_resource {
            res.reconfigure(piece_length, total_length);
        }
    }

    /// Check whether this connection has an active session resource.
    pub fn is_active(&self) -> bool {
        self.session_resource.is_some()
    }

    // -----------------------------------------------------------------------
    // Bitfield delegation (convenience methods)
    // -----------------------------------------------------------------------

    /// Check whether the peer has a given piece.
    ///
    /// Delegates to [`PeerSessionResource::has_piece`]. Returns `false` if
    /// no session resource is allocated.
    pub fn has_piece(&self, index: usize) -> bool {
        self.session_resource
            .as_ref()
            .map_or(false, |r| r.has_piece(index))
    }

    /// Set the peer bitfield from raw bytes.
    ///
    /// Delegates to [`PeerSessionResource::set_bitfield`]. No-op if no
    /// session resource is allocated.
    pub fn set_peer_bitfield(&mut self, bitfield: &[u8]) {
        if let Some(ref mut res) = self.session_resource {
            res.set_bitfield(bitfield);
        }
    }

    /// Update the peer bitfield: set (operation=1) or clear (operation=0)
    /// the bit at `index`.
    ///
    /// Delegates to [`PeerSessionResource::update_bitfield`]. No-op if no
    /// session resource is allocated.
    pub fn update_peer_bitfield(&mut self, index: usize, operation: i32) {
        if let Some(ref mut res) = self.session_resource {
            res.update_bitfield(index, operation);
        }
    }

    /// Mark the peer as a seeder (has all pieces).
    ///
    /// Delegates to [`PeerSessionResource::mark_seeder`]. No-op if no
    /// session resource is allocated.
    pub fn mark_seeder(&mut self) {
        self.seeder = true;
        if let Some(ref mut res) = self.session_resource {
            res.mark_seeder();
        }
    }

    // -----------------------------------------------------------------------
    // Keep-alive management
    // -----------------------------------------------------------------------

    /// Check whether we should send a keep-alive message.
    ///
    /// Returns `true` if more than [`KEEPALIVE_INTERVAL_SECS`] have elapsed
    /// since the last keep-alive was sent.
    pub fn should_send_keepalive(&self) -> bool {
        self.last_keepalive_sent.elapsed() >= Duration::from_secs(KEEPALIVE_INTERVAL_SECS)
    }

    /// Check whether the peer has timed out (no messages received for
    /// [`PEER_TIMEOUT_SECS`]).
    pub fn is_peer_timed_out(&self) -> bool {
        self.last_message_received.elapsed() >= Duration::from_secs(PEER_TIMEOUT_SECS)
    }

    /// Send a keep-alive message (4-byte zero-length prefix).
    ///
    /// Also updates `last_keepalive_sent`.
    pub async fn send_keepalive(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;
        let data = serialize(&BtMessage::KeepAlive);
        self.write_raw(&data).await?;
        self.last_keepalive_sent = Instant::now();
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Send buffering
    // -----------------------------------------------------------------------

    /// Queue a serialized message into the send buffer without flushing.
    ///
    /// Call [`flush_send_buffer`](Self::flush_send_buffer) later to actually
    /// write the data to the socket. This allows batching multiple small
    /// messages into a single write.
    pub fn queue_message(&mut self, data: Vec<u8>) {
        self.send_buffer.push_bytes(data);
    }

    /// Flush all queued messages in the send buffer to the socket.
    pub async fn flush_send_buffer(&mut self) -> Result<()> {
        if self.send_buffer.is_empty() {
            return Ok(());
        }
        let data = self.send_buffer.take_pending();
        self.write_raw(&data).await?;
        self.last_keepalive_sent = Instant::now();
        Ok(())
    }

    /// Get a reference to the send buffer (for inspection).
    pub fn send_buffer(&self) -> &SendBuffer {
        &self.send_buffer
    }

    /// Get a mutable reference to the send buffer.
    pub fn send_buffer_mut(&mut self) -> &mut SendBuffer {
        &mut self.send_buffer
    }

    // -----------------------------------------------------------------------
    // PEX (BEP 11) — inbound peer accumulation
    // -----------------------------------------------------------------------

    /// Drain all accumulated PEX-discovered peers from this connection.
    ///
    /// Called by the download loop after each iteration to process peers
    /// discovered via incoming ut_pex messages during block reads.
    pub fn drain_pex_peers(
        &mut self,
    ) -> Vec<aria2_protocol::bittorrent::peer::connection::PeerAddr> {
        std::mem::take(&mut self.pending_pex_peers)
    }

    // -----------------------------------------------------------------------
    // Message receipt bookkeeping
    // -----------------------------------------------------------------------

    /// Update the last-message-received timestamp to now.
    ///
    /// Should be called whenever a message is successfully read from the
    /// peer, so that [`is_peer_timed_out`](Self::is_peer_timed_out) works
    /// correctly.
    pub fn on_message_received(&mut self) {
        self.last_message_received = Instant::now();
    }

    // -----------------------------------------------------------------------
    // Protocol message senders (immediate flush — preserved API)
    // -----------------------------------------------------------------------

    pub async fn send_unchoke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Unchoke;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_choke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_choke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Choke;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_interested(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Interested;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_not_interested(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_not_interested().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::NotInterested;
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_have(&mut self, piece_index: u32) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_have(piece_index).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Have { piece_index };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_request(
        &mut self,
        req: aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_request(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
                let msg = BtMessage::Request {
                    request: PieceBlockRequest::new(req.index, req.begin, req.length),
                };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_cancel(
        &mut self,
        req: &aria2_protocol::bittorrent::message::types::PieceBlockRequest,
    ) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_cancel(req).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
                let msg = BtMessage::Cancel {
                    request: PieceBlockRequest::new(req.index, req.begin, req.length),
                };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    pub async fn send_bitfield(&mut self, bitfield: Vec<u8>) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_bitfield(bitfield).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                use aria2_protocol::bittorrent::message::serializer::serialize;
                use aria2_protocol::bittorrent::message::types::BtMessage;
                let msg = BtMessage::Bitfield { data: bitfield };
                c.send_message(&serialize(&msg)).await
            }
        }
    }

    /// Send a HaveAll message (BEP 6 Fast Extension).
    /// Indicates that the sender has all pieces.
    pub async fn send_have_all(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;
        let msg = BtMessage::HaveAll;
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_message(&serialize(&msg)).await,
        }
    }

    /// Send a HaveNone message (BEP 6 Fast Extension).
    /// Indicates that the sender has no pieces.
    pub async fn send_have_none(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;
        let msg = BtMessage::HaveNone;
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_message(&msg).await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => c.send_message(&serialize(&msg)).await,
        }
    }

    // -----------------------------------------------------------------------
    // Message reading
    // -----------------------------------------------------------------------

    pub async fn read_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        let result = match &mut self.inner {
            InnerConnection::Plain(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                let msg_bytes = c.recv_message().await?;
                if let Some(bytes) = msg_bytes {
                    use aria2_protocol::bittorrent::message::factory::parse_message;
                    parse_message(&bytes).map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))
                } else {
                    Ok(None)
                }
            }
        };
        // Update keep-alive tracking on any successful message receipt
        if result.is_ok() {
            self.on_message_received();
        }
        result
    }

    // -----------------------------------------------------------------------
    // Connection state queries
    // -----------------------------------------------------------------------

    pub fn is_connected(&self) -> bool {
        match &self.inner {
            InnerConnection::Plain(c) => c.is_connected(),
            InnerConnection::Encrypted(c) => c.is_connected(),
            InnerConnection::Utp(c) => c.is_connected(),
        }
    }

    pub fn is_encrypted(&self) -> bool {
        matches!(self.inner, InnerConnection::Encrypted(_))
    }

    // -----------------------------------------------------------------------
    // Low-level write helper
    // -----------------------------------------------------------------------

    /// Write raw bytes directly to the inner connection.
    ///
    /// This is the single point of actual socket write used by both the
    /// immediate-flush `send_*` methods and the `flush_send_buffer` path.
    async fn write_raw(&mut self, data: &[u8]) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => {
                // PeerConnection exposes send_message(&BtMessage) as the
                // high-level path. For the flush path we split the buffer
                // into individual BT messages and send each one.  This is
                // acceptable because flush is called infrequently.
                Self::flush_raw_to_plain(c, data).await
            }
            InnerConnection::Encrypted(c) => Self::flush_raw_to_encrypted(c, data).await,
            InnerConnection::Utp(c) => c.send_message(data).await,
        }
    }

    /// Flush raw bytes through a plain TCP connection by splitting them
    /// into individual BT messages.
    async fn flush_raw_to_plain(
        conn: &mut aria2_protocol::bittorrent::peer::connection::PeerConnection,
        data: &[u8],
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::factory::parse_message_stream;
        let messages = parse_message_stream(data);
        for (msg, _size) in messages {
            if let Some(bt_msg) = msg {
                conn.send_message(&bt_msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                })?;
            }
        }
        Ok(())
    }

    /// Flush raw bytes through an encrypted connection.
    async fn flush_raw_to_encrypted(
        conn: &mut aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection,
        data: &[u8],
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::factory::parse_message_stream;
        let messages = parse_message_stream(data);
        for (msg, _size) in messages {
            if let Some(bt_msg) = msg {
                conn.send_message(&bt_msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e,
                    })
                })?;
            }
        }
        Ok(())
    }
}
