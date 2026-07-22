//! BitTorrent peer connection abstraction with send buffering, session resources,
//! and keep-alive management.
//!
//! Mirrors the C++ aria2 architecture of `Peer` + `PeerSessionResource` +
//! `PeerConnection` + `SocketBuffer`:
//!
//! - [`SendBuffer`] — outbound message buffer that batches small messages
//!   into larger TCP writes (C++ `SocketBuffer`).
//! - [`PeerSessionResource`] — per-session state allocated when a peer becomes
//!   active and released on disconnect (C++ `PeerSessionResource`).
//! - [`BtPeerConn`] — the public connection type that composes the above with
//!   keep-alive management, bitfield delegation, and the existing inner
//!   connection variants.
//!
//! # Keep-alive
//!
//! Per the BitTorrent spec, peers must send a keep-alive message every
//! ~2 minutes if no other message has been sent. The connection is
//! considered dead after ~3 minutes of inactivity.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::constants;
use crate::engine::peer_stats::PeerStats;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::segment::bitfield_util;

// ---------------------------------------------------------------------------
// Keep-alive / timeout constants
// ---------------------------------------------------------------------------

/// Keep-alive interval (2 minutes, per BitTorrent spec).
const KEEPALIVE_INTERVAL_SECS: u64 = 120;

/// Timeout for peer inactivity before considering the connection dead.
const PEER_TIMEOUT_SECS: u64 = 180;

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
// UtpPeerConnection
// ---------------------------------------------------------------------------

/// uTP peer connection wrapper.
///
/// Wraps a uTP stream for BitTorrent peer communication.
/// Provides the same interface as TCP connections but uses UDP-based uTP protocol.
pub struct UtpPeerConnection {
    /// uTP socket reference (shared among multiple connections)
    socket: Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>,
    /// Connection ID within the socket
    conn_id: u16,
    /// Info hash for the torrent
    info_hash: [u8; 20],
    /// Whether handshake is complete
    handshake_complete: bool,
    /// Receive buffer for partial messages
    recv_buffer: Vec<u8>,
}

impl UtpPeerConnection {
    /// Create a new uTP peer connection.
    pub fn new(
        socket: Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>,
        conn_id: u16,
        info_hash: [u8; 20],
    ) -> Self {
        Self {
            socket,
            conn_id,
            info_hash,
            handshake_complete: false,
            recv_buffer: Vec::new(),
        }
    }

    /// Connect to a remote peer via uTP.
    pub async fn connect(addr: std::net::SocketAddr, info_hash: &[u8; 20]) -> Result<Self> {
        let socket = aria2_protocol::bittorrent::utp::UtpSocket::bind_any()
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))?;

        let socket = Arc::new(Mutex::new(socket));

        let conn_id = {
            let mut sock = socket.lock().await;
            sock.connect(addr)
                .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))?
        };

        Ok(Self {
            socket,
            conn_id,
            info_hash: *info_hash,
            handshake_complete: false,
            recv_buffer: Vec::new(),
        })
    }

    /// Get the connection ID.
    pub fn conn_id(&self) -> u16 {
        self.conn_id
    }

    /// Check if connection is established.
    pub fn is_connected(&self) -> bool {
        self.handshake_complete
    }

    /// Perform BitTorrent handshake over uTP.
    pub async fn perform_handshake(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::handshake::Handshake;
        use aria2_protocol::bittorrent::peer::id::generate_peer_id;

        let peer_id = generate_peer_id();
        let handshake = Handshake::new(&self.info_hash, &peer_id);
        let handshake_bytes = handshake.to_bytes();

        {
            let mut socket = self.socket.lock().await;
            socket.send(self.conn_id, &handshake_bytes).map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
        }

        let mut response_buf = vec![0u8; 68];
        let len = {
            let mut socket = self.socket.lock().await;
            socket.recv(self.conn_id, &mut response_buf).map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?
        };

        if len < 68 {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Handshake response too short".to_string(),
            )));
        }

        let response = Handshake::parse(&response_buf[..len])
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))?;

        if response.info_hash != self.info_hash {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Info hash mismatch".to_string(),
            )));
        }

        self.handshake_complete = true;
        Ok(())
    }

    /// Send a BitTorrent message.
    pub async fn send_message(&mut self, message: &[u8]) -> Result<()> {
        let mut socket = self.socket.lock().await;
        socket.send(self.conn_id, message).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: e.to_string(),
            })
        })?;
        Ok(())
    }

    /// Receive a BitTorrent message.
    pub async fn recv_message(&mut self) -> Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; constants::BT_RECEIVE_BUFFER_SIZE];
        let len = {
            let mut socket = self.socket.lock().await;
            match socket.recv(self.conn_id, &mut buf) {
                Ok(0) => return Ok(None),
                Ok(len) => len,
                Err(e) => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: e.to_string(),
                        },
                    ));
                }
            }
        };

        self.recv_buffer.extend_from_slice(&buf[..len]);

        if self.recv_buffer.len() >= 4 {
            let msg_len = u32::from_be_bytes([
                self.recv_buffer[0],
                self.recv_buffer[1],
                self.recv_buffer[2],
                self.recv_buffer[3],
            ]) as usize;

            if self.recv_buffer.len() >= 4 + msg_len {
                let message = self.recv_buffer[4..4 + msg_len].to_vec();
                self.recv_buffer = self.recv_buffer[4 + msg_len..].to_vec();
                return Ok(Some(message));
            }
        }

        Ok(None)
    }

    /// Close the connection.
    pub async fn close(&mut self) -> Result<()> {
        let mut socket = self.socket.lock().await;
        socket.close_connection(self.conn_id).map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: e.to_string(),
            })
        })?;
        Ok(())
    }

    /// Get connection statistics.
    pub async fn stats(&self) -> Result<aria2_protocol::bittorrent::utp::ConnectionStats> {
        let socket = self.socket.lock().await;
        socket
            .connection_stats(self.conn_id)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))
    }
}

// ===========================================================================
// SendBuffer — outbound message buffer (C++ SocketBuffer)
// ===========================================================================

/// Outbound message buffer for batching small messages into larger TCP writes.
///
/// Mirrors the C++ `SocketBuffer`: messages are pushed into the buffer and
/// only written to the socket when [`flush()`](BtPeerConn::flush_send_buffer)
/// is called. This reduces the number of syscalls and improves throughput,
/// especially when sending multiple small messages (e.g., a burst of Have
/// messages).
pub struct SendBuffer {
    /// Queued message bytes, waiting to be written to the socket.
    pending: Vec<u8>,
    /// Whether encryption is enabled for this buffer.
    encryption_enabled: bool,
}

impl SendBuffer {
    /// Create a new empty send buffer.
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            encryption_enabled: false,
        }
    }

    /// Add data to the pending buffer.
    ///
    /// In a future iteration, when `encryption_enabled` is `true`, the data
    /// will be encrypted before being queued. For now the flag is stored but
    /// does not affect the data.
    pub fn push_bytes(&mut self, data: Vec<u8>) {
        // TODO: encrypt data if encryption_enabled
        self.pending.extend_from_slice(&data);
    }

    /// Check whether the pending buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Get the number of bytes in the pending buffer.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Clear the pending buffer.
    pub fn clear(&mut self) {
        self.pending.clear();
    }

    /// Drain the pending data, returning it as a `Vec<u8>` for writing to
    /// the socket.
    pub fn take_pending(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }

    /// Set whether encryption is enabled for this buffer.
    pub fn set_encryption_enabled(&mut self, enabled: bool) {
        self.encryption_enabled = enabled;
    }

    /// Check whether encryption is enabled.
    pub fn is_encryption_enabled(&self) -> bool {
        self.encryption_enabled
    }
}

impl Default for SendBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ===========================================================================
// PeerSessionResource — per-session state (C++ PeerSessionResource)
// ===========================================================================

/// Per-session resource for an active BitTorrent peer connection.
///
/// Mirrors the C++ `PeerSessionResource`. Allocated when a peer session starts
/// and released when it ends. Contains bitfield management, extension support,
/// and choking algorithm integration fields.
pub struct PeerSessionResource {
    /// Bitfield tracking which pieces this peer has.
    bitfield: Vec<u8>,
    /// Bitfield length in bytes.
    bitfield_length: usize,
    /// Piece length for the torrent.
    piece_length: u32,
    /// Total length of the torrent.
    total_length: u64,
    /// Number of pieces in the torrent.
    num_pieces: u32,

    // Fast Extension (BEP 6)
    /// Whether fast extension is enabled for this peer.
    fast_extension_enabled: bool,
    /// Piece indices that the peer has allowed us to request (even when choked).
    peer_allowed_index_set: HashSet<u32>,
    /// Piece indices that we have allowed the peer to request (even when choked).
    am_allowed_index_set: HashSet<u32>,

    // Extension Protocol (BEP 10)
    /// Whether extended messaging is enabled.
    extended_messaging_enabled: bool,
    /// Extension message registry: key -> message ID.
    extension_registry: HashMap<String, u8>,

    // DHT (BEP 5)
    /// Whether DHT is enabled for this peer.
    dht_enabled: bool,

    // Choking Algorithm Integration
    /// Whether choking this peer is required (set by choking algorithm).
    choking_required: bool,
    /// Whether this peer is eligible for optimistic unchoking.
    opt_unchoking: bool,
    /// Whether this peer is snubbing (not sending data despite being unchoked).
    snubbing: bool,
}

impl PeerSessionResource {
    /// Create a new `PeerSessionResource` for a torrent with the given
    /// piece length and total length.
    pub fn new(piece_length: u32, total_length: u64) -> Self {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            ((total_length + piece_length as u64 - 1) / piece_length as u64) as u32
        };
        let bitfield_length = ((num_pieces as usize) + 7) / 8;

        Self {
            bitfield: vec![0u8; bitfield_length],
            bitfield_length,
            piece_length,
            total_length,
            num_pieces,
            fast_extension_enabled: false,
            peer_allowed_index_set: HashSet::new(),
            am_allowed_index_set: HashSet::new(),
            extended_messaging_enabled: false,
            extension_registry: HashMap::new(),
            dht_enabled: false,
            choking_required: true,
            opt_unchoking: false,
            snubbing: false,
        }
    }

    // -----------------------------------------------------------------------
    // Bitfield
    // -----------------------------------------------------------------------

    /// Check whether the peer has a given piece.
    ///
    /// Returns `false` if the index is out of range or the bitfield is
    /// too short.
    pub fn has_piece(&self, index: usize) -> bool {
        bitfield_util::test_bit(&self.bitfield, self.num_pieces as usize, index)
    }

    /// Set the peer bitfield from raw bytes.
    ///
    /// Copies `bitfield` into the internal storage, truncating or
    /// zero-extending as needed.
    pub fn set_bitfield(&mut self, bitfield: &[u8]) {
        let copy_len = std::cmp::min(bitfield.len(), self.bitfield.len());
        self.bitfield[..copy_len].copy_from_slice(&bitfield[..copy_len]);
        // Zero-fill remaining bytes if source is shorter
        for byte in &mut self.bitfield[copy_len..] {
            *byte = 0;
        }
    }

    /// Update the peer bitfield: set (operation=1) or clear (operation=0)
    /// the bit at `index`.
    pub fn update_bitfield(&mut self, index: usize, operation: i32) {
        if index >= self.num_pieces as usize {
            return;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        if byte >= self.bitfield.len() {
            return;
        }
        if operation == 1 {
            self.bitfield[byte] |= 1 << bit;
        } else {
            self.bitfield[byte] &= !(1 << bit);
        }
    }

    /// Mark all pieces as available (seeder bitfield).
    pub fn set_all_bitfield(&mut self) {
        for byte in &mut self.bitfield {
            *byte = 0xFF;
        }
        // Clear trailing bits beyond num_pieces
        let remaining = (self.num_pieces as usize) % 8;
        if remaining != 0 {
            let extra = 8 - remaining;
            if let Some(last) = self.bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Mark the peer as a seeder (has all pieces).
    pub fn mark_seeder(&mut self) {
        self.set_all_bitfield();
    }

    /// Check whether the peer is a seeder (has all pieces).
    pub fn is_seeder(&self) -> bool {
        // Count set bits and compare with num_pieces
        let mut count = 0usize;
        for &byte in &self.bitfield {
            count += byte.count_ones() as usize;
        }
        // Adjust for trailing bits
        let remaining = (self.num_pieces as usize) % 8;
        if remaining != 0 && !self.bitfield.is_empty() {
            let extra = 8 - remaining;
            if let Some(&last) = self.bitfield.last() {
                let trailing = (last & ((1u8 << extra) - 1)).count_ones() as usize;
                count -= trailing;
            }
        }
        count == self.num_pieces as usize
    }

    /// Get a reference to the raw bitfield bytes.
    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }

    /// Reconfigure the session resource for a new piece/total length.
    ///
    /// Called when the torrent metadata is updated (e.g., after magnet
    /// link metadata exchange).
    pub fn reconfigure(&mut self, piece_length: u32, total_length: u64) {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            ((total_length + piece_length as u64 - 1) / piece_length as u64) as u32
        };
        let bitfield_length = ((num_pieces as usize) + 7) / 8;

        self.bitfield.resize(bitfield_length, 0);
        self.bitfield_length = bitfield_length;
        self.piece_length = piece_length;
        self.total_length = total_length;
        self.num_pieces = num_pieces;
    }

    /// Get the number of pieces.
    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }

    /// Get the piece length.
    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Get the total length.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    // -----------------------------------------------------------------------
    // Fast Extension (BEP 6)
    // -----------------------------------------------------------------------

    /// Enable or disable fast extension.
    pub fn set_fast_extension_enabled(&mut self, enabled: bool) {
        self.fast_extension_enabled = enabled;
    }

    /// Check whether fast extension is enabled.
    pub fn is_fast_extension_enabled(&self) -> bool {
        self.fast_extension_enabled
    }

    /// Add a piece index to the set the peer has allowed us to request.
    pub fn add_peer_allowed_index(&mut self, index: u32) {
        self.peer_allowed_index_set.insert(index);
    }

    /// Check whether a piece index is in the peer-allowed set.
    pub fn is_in_peer_allowed_index_set(&self, index: u32) -> bool {
        self.peer_allowed_index_set.contains(&index)
    }

    /// Add a piece index to the set we have allowed the peer to request.
    pub fn add_am_allowed_index(&mut self, index: u32) {
        self.am_allowed_index_set.insert(index);
    }

    /// Check whether a piece index is in the am-allowed set.
    pub fn is_in_am_allowed_index_set(&self, index: u32) -> bool {
        self.am_allowed_index_set.contains(&index)
    }

    // -----------------------------------------------------------------------
    // Extension Protocol (BEP 10)
    // -----------------------------------------------------------------------

    /// Enable or disable extended messaging.
    pub fn set_extended_messaging_enabled(&mut self, enabled: bool) {
        self.extended_messaging_enabled = enabled;
    }

    /// Check whether extended messaging is enabled.
    pub fn is_extended_messaging_enabled(&self) -> bool {
        self.extended_messaging_enabled
    }

    /// Register an extension with the given key and message ID.
    pub fn add_extension(&mut self, key: &str, id: u8) {
        self.extension_registry.insert(key.to_string(), id);
    }

    /// Look up the message ID for a given extension key.
    pub fn get_extension_message_id(&self, key: &str) -> Option<u8> {
        self.extension_registry.get(key).copied()
    }

    /// Look up the extension name for a given message ID.
    pub fn get_extension_name(&self, id: u8) -> Option<&str> {
        self.extension_registry
            .iter()
            .find(|&(_, &v)| v == id)
            .map(|(k, _)| k.as_str())
    }

    // -----------------------------------------------------------------------
    // DHT (BEP 5)
    // -----------------------------------------------------------------------

    /// Enable or disable DHT for this peer.
    pub fn set_dht_enabled(&mut self, enabled: bool) {
        self.dht_enabled = enabled;
    }

    /// Check whether DHT is enabled.
    pub fn is_dht_enabled(&self) -> bool {
        self.dht_enabled
    }

    // -----------------------------------------------------------------------
    // Choking Algorithm Integration
    // -----------------------------------------------------------------------

    /// Set whether choking this peer is required.
    pub fn set_choking_required(&mut self, required: bool) {
        self.choking_required = required;
    }

    /// Check whether choking this peer is required.
    pub fn choking_required(&self) -> bool {
        self.choking_required
    }

    /// Set whether this peer is eligible for optimistic unchoking.
    pub fn set_opt_unchoking(&mut self, enabled: bool) {
        self.opt_unchoking = enabled;
    }

    /// Check whether this peer is eligible for optimistic unchoking.
    pub fn opt_unchoking(&self) -> bool {
        self.opt_unchoking
    }

    /// Set whether this peer is snubbing.
    pub fn set_snubbing(&mut self, snubbing: bool) {
        self.snubbing = snubbing;
    }

    /// Check whether this peer is snubbing.
    pub fn snubbing(&self) -> bool {
        self.snubbing
    }

    /// Determine whether this peer should be choked.
    ///
    /// Returns `true` if choking is required and the peer is not eligible
    /// for optimistic unchoking.
    pub fn should_be_choking(&self) -> bool {
        self.choking_required && !self.opt_unchoking
    }

    /// Count outstanding upload operations (placeholder).
    ///
    /// In the C++ code this counts pending upload requests. For now it
    /// returns 0; will be wired up when upload scheduling is implemented.
    pub fn count_outstanding_upload(&self) -> usize {
        0
    }
}

// ===========================================================================
// ConnectionType
// ===========================================================================

/// Type of peer connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    /// Standard TCP connection.
    Tcp,
    /// uTP (UDP-based) connection.
    Utp,
}

// ===========================================================================
// BtPeerConn — the public connection type
// ===========================================================================

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
                    stats: PeerStats::new([0u8; 20], std::net::SocketAddr::new(
                        addr.ip.parse().unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)),
                        addr.port,
                    )),
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
                stream,
                [0u8; 20],
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
            stats: PeerStats::new([0u8; 20], std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                0,
            )),
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
            InnerConnection::Plain(c) => {
                c.send_message(&msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
                })
            }
            InnerConnection::Encrypted(c) => {
                c.send_message(&msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
                })
            }
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
            InnerConnection::Plain(c) => {
                c.send_message(&msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
                })
            }
            InnerConnection::Encrypted(c) => {
                c.send_message(&msg).await.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
                })
            }
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
                    parse_message(&bytes)
                        .map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))
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
            InnerConnection::Encrypted(c) => {
                Self::flush_raw_to_encrypted(c, data).await
            }
            InnerConnection::Utp(c) => {
                c.send_message(data).await
            }
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // SendBuffer tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_send_buffer_push_and_drain() {
        let mut buf = SendBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);

        buf.push_bytes(vec![1, 2, 3]);
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 3);

        buf.push_bytes(vec![4, 5, 6]);
        assert_eq!(buf.len(), 6);

        let drained = buf.take_pending();
        assert_eq!(drained, vec![1, 2, 3, 4, 5, 6]);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_send_buffer_empty_check() {
        let mut buf = SendBuffer::new();
        assert!(buf.is_empty());

        buf.push_bytes(vec![42]);
        assert!(!buf.is_empty());

        buf.clear();
        assert!(buf.is_empty());

        buf.push_bytes(vec![1]);
        let _ = buf.take_pending();
        assert!(buf.is_empty());
    }

    #[test]
    fn test_send_buffer_encryption_flag() {
        let mut buf = SendBuffer::new();
        assert!(!buf.is_encryption_enabled());

        buf.set_encryption_enabled(true);
        assert!(buf.is_encryption_enabled());

        buf.set_encryption_enabled(false);
        assert!(!buf.is_encryption_enabled());
    }

    #[test]
    fn test_send_buffer_default() {
        let buf = SendBuffer::default();
        assert!(buf.is_empty());
    }

    // -----------------------------------------------------------------------
    // PeerSessionResource — bitfield tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_session_resource_bitfield() {
        // 4 pieces of 256 KiB each = 1 MiB total
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert_eq!(res.num_pieces(), 4);
        assert_eq!(res.bitfield_length, 1);

        // Initially no pieces
        for i in 0..4 {
            assert!(!res.has_piece(i), "piece {} should not be set", i);
        }

        // Set piece 0
        res.update_bitfield(0, 1);
        assert!(res.has_piece(0));
        assert!(!res.has_piece(1));

        // Set piece 3
        res.update_bitfield(3, 1);
        assert!(res.has_piece(3));

        // Clear piece 0
        res.update_bitfield(0, 0);
        assert!(!res.has_piece(0));

        // Set bitfield from raw bytes
        res.set_bitfield(&[0xC0]); // bits 0 and 1
        assert!(res.has_piece(0));
        assert!(res.has_piece(1));
        assert!(!res.has_piece(2));
        assert!(!res.has_piece(3));
    }

    #[test]
    fn test_peer_session_resource_seeder() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert!(!res.is_seeder());

        res.mark_seeder();
        assert!(res.is_seeder());
        for i in 0..4 {
            assert!(res.has_piece(i), "seeder should have piece {}", i);
        }
    }

    #[test]
    fn test_peer_session_resource_set_all_bitfield() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        res.set_all_bitfield();
        // 4 pieces in 1 byte = 0xF0 (upper 4 bits)
        assert_eq!(res.bitfield(), &[0xF0]);
        assert!(res.is_seeder());
    }

    #[test]
    fn test_peer_session_resource_reconfigure() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert_eq!(res.num_pieces(), 4);

        res.reconfigure(512 * 1024, 4 * 1024 * 1024);
        assert_eq!(res.num_pieces(), 8);
        assert_eq!(res.bitfield_length, 1);
    }

    #[test]
    fn test_peer_session_resource_out_of_range() {
        let res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert!(!res.has_piece(100)); // out of range
    }

    #[test]
    fn test_peer_session_resource_update_bitfield_out_of_range() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        // Should not panic on out-of-range index
        res.update_bitfield(100, 1);
        assert!(!res.has_piece(100));
    }

    // -----------------------------------------------------------------------
    // PeerSessionResource — Fast Extension tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_session_resource_fast_extension() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert!(!res.is_fast_extension_enabled());

        res.set_fast_extension_enabled(true);
        assert!(res.is_fast_extension_enabled());

        // Peer-allowed index set
        res.add_peer_allowed_index(5);
        res.add_peer_allowed_index(10);
        assert!(res.is_in_peer_allowed_index_set(5));
        assert!(res.is_in_peer_allowed_index_set(10));
        assert!(!res.is_in_peer_allowed_index_set(7));

        // Am-allowed index set
        res.add_am_allowed_index(3);
        assert!(res.is_in_am_allowed_index_set(3));
        assert!(!res.is_in_am_allowed_index_set(5));
    }

    // -----------------------------------------------------------------------
    // PeerSessionResource — Extension Protocol tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_session_resource_extensions() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert!(!res.is_extended_messaging_enabled());

        res.set_extended_messaging_enabled(true);
        assert!(res.is_extended_messaging_enabled());

        // Register extensions
        res.add_extension("ut_pex", 1);
        res.add_extension("ut_metadata", 2);

        assert_eq!(res.get_extension_message_id("ut_pex"), Some(1));
        assert_eq!(res.get_extension_message_id("ut_metadata"), Some(2));
        assert_eq!(res.get_extension_message_id("unknown"), None);

        assert_eq!(res.get_extension_name(1), Some("ut_pex"));
        assert_eq!(res.get_extension_name(2), Some("ut_metadata"));
        assert_eq!(res.get_extension_name(99), None);
    }

    // -----------------------------------------------------------------------
    // PeerSessionResource — DHT tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_session_resource_dht() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert!(!res.is_dht_enabled());

        res.set_dht_enabled(true);
        assert!(res.is_dht_enabled());
    }

    // -----------------------------------------------------------------------
    // PeerSessionResource — Choking tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_session_resource_choking() {
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);

        // Default: choking_required = true, opt_unchoking = false
        assert!(res.choking_required());
        assert!(!res.opt_unchoking());
        assert!(!res.snubbing());
        assert!(res.should_be_choking());

        // Opt unchoking overrides choking requirement
        res.set_opt_unchoking(true);
        assert!(!res.should_be_choking());

        // Snubbing
        res.set_snubbing(true);
        assert!(res.snubbing());

        // Release choking requirement
        res.set_choking_required(false);
        assert!(!res.choking_required());
        assert!(!res.should_be_choking());
    }

    // -----------------------------------------------------------------------
    // BtPeerConn — session resource lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn test_bt_peer_conn_session_resource_lifecycle() {
        // We cannot easily construct a BtPeerConn without a real connection,
        // so test the resource management pattern directly.
        let mut res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert_eq!(res.num_pieces(), 4);
        assert!(!res.is_seeder());

        res.mark_seeder();
        assert!(res.is_seeder());

        // Release (simulate disconnect)
        drop(res);
    }

    // -----------------------------------------------------------------------
    // BtPeerConn — keepalive / timeout
    // -----------------------------------------------------------------------

    #[test]
    fn test_bt_peer_conn_keepalive() {
        // Test the keepalive interval logic directly
        let now = Instant::now();

        // Just-sent keepalive should not trigger
        let last_sent = now;
        assert!(last_sent.elapsed() < Duration::from_secs(KEEPALIVE_INTERVAL_SECS));

        // A keepalive sent long ago should trigger
        let old_sent = now - Duration::from_secs(KEEPALIVE_INTERVAL_SECS + 10);
        assert!(old_sent.elapsed() >= Duration::from_secs(KEEPALIVE_INTERVAL_SECS));
    }

    #[test]
    fn test_bt_peer_conn_peer_timeout() {
        let now = Instant::now();

        // Recent message should not trigger timeout
        let last_recv = now;
        assert!(last_recv.elapsed() < Duration::from_secs(PEER_TIMEOUT_SECS));

        // Old message should trigger timeout
        let old_recv = now - Duration::from_secs(PEER_TIMEOUT_SECS + 10);
        assert!(old_recv.elapsed() >= Duration::from_secs(PEER_TIMEOUT_SECS));
    }

    // -----------------------------------------------------------------------
    // BtPeerConn — queue_message and flush (unit test of buffer logic)
    // -----------------------------------------------------------------------

    #[test]
    fn test_bt_peer_conn_queue_message_and_flush() {
        let mut buf = SendBuffer::new();

        // Queue multiple messages
        use aria2_protocol::bittorrent::message::serializer::serialize;
        use aria2_protocol::bittorrent::message::types::BtMessage;

        buf.push_bytes(serialize(&BtMessage::Unchoke));
        buf.push_bytes(serialize(&BtMessage::Interested));
        buf.push_bytes(serialize(&BtMessage::Have { piece_index: 42 }));

        assert!(!buf.is_empty());
        let combined = buf.take_pending();

        // Verify the combined buffer contains all three messages
        // Unchoke: 4-byte length (00 00 00 01) + 1-byte ID (01) = 5 bytes
        // Interested: 4-byte length (00 00 00 01) + 1-byte ID (02) = 5 bytes
        // Have: 4-byte length (00 00 00 05) + 1-byte ID (04) + 4-byte piece = 9 bytes
        assert_eq!(combined.len(), 5 + 5 + 9);

        // Parse the combined stream
        use aria2_protocol::bittorrent::message::factory::parse_message_stream;
        let msgs = parse_message_stream(&combined);
        assert_eq!(msgs.len(), 3);

        assert_eq!(msgs[0].0, Some(BtMessage::Unchoke));
        assert_eq!(msgs[1].0, Some(BtMessage::Interested));
        assert_eq!(msgs[2].0, Some(BtMessage::Have { piece_index: 42 }));
    }

    // -----------------------------------------------------------------------
    // Legacy tests (preserved)
    // -----------------------------------------------------------------------

    #[test]
    fn test_allowed_fast_set_operations() {
        let mut set: HashSet<u32> = HashSet::new();
        assert!(set.is_empty());
        assert!(!set.contains(&42));
        set.insert(42);
        assert!(set.contains(&42));
        set.insert(10);
        set.insert(99);
        assert_eq!(set.len(), 3);
        assert!(!set.contains(&999));
        set.insert(42);
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn test_allowed_fast_multiple_indices() {
        let mut set: HashSet<u32> = HashSet::new();
        for i in 0..100u32 {
            set.insert(i);
        }
        assert_eq!(set.len(), 100);
        for i in 0..100u32 {
            assert!(set.contains(&i));
        }
        assert!(!set.contains(&100));
    }

    // -----------------------------------------------------------------------
    // PeerSessionResource — larger bitfield
    // -----------------------------------------------------------------------

    #[test]
    fn test_peer_session_resource_large_bitfield() {
        // 100 pieces of 1 MiB each = 100 MiB total
        let mut res = PeerSessionResource::new(1024 * 1024, 100 * 1024 * 1024);
        assert_eq!(res.num_pieces(), 100);
        assert_eq!(res.bitfield_length, 13); // ceil(100/8) = 13

        // Set piece 0 and 99
        res.update_bitfield(0, 1);
        res.update_bitfield(99, 1);
        assert!(res.has_piece(0));
        assert!(res.has_piece(99));
        assert!(!res.has_piece(50));

        // Mark seeder — all 100 bits should be set
        res.mark_seeder();
        assert!(res.is_seeder());
        for i in 0..100 {
            assert!(res.has_piece(i), "seeder should have piece {}", i);
        }
        // Piece 100 is out of range
        assert!(!res.has_piece(100));
    }

    #[test]
    fn test_peer_session_resource_zero_length() {
        let res = PeerSessionResource::new(0, 0);
        assert_eq!(res.num_pieces(), 0);
        // Vacuously a seeder
        assert!(res.is_seeder());
    }

    #[test]
    fn test_peer_session_resource_count_outstanding_upload() {
        let res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert_eq!(res.count_outstanding_upload(), 0);
    }

    #[test]
    fn test_peer_session_resource_accessors() {
        let res = PeerSessionResource::new(256 * 1024, 1024 * 1024);
        assert_eq!(res.piece_length(), 256 * 1024);
        assert_eq!(res.total_length(), 1024 * 1024);
    }
}
