use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

#[allow(clippy::large_enum_variant)]
pub(crate) enum InnerConnection {
    Plain(aria2_protocol::bittorrent::peer::connection::PeerConnection),
    Encrypted(aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection),
    Utp(UtpPeerConnection),
}

/// uTP peer connection wrapper
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
    /// Create a new uTP peer connection
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

    /// Connect to a remote peer via uTP
    pub async fn connect(
        addr: std::net::SocketAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        // Create a new uTP socket for this connection
        let socket = aria2_protocol::bittorrent::utp::UtpSocket::bind_any()
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))?;
        
        let socket = Arc::new(Mutex::new(socket));
        
        // Initiate uTP connection
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

    /// Get the connection ID
    pub fn conn_id(&self) -> u16 {
        self.conn_id
    }

    /// Check if connection is established
    pub fn is_connected(&self) -> bool {
        // This is a simplified check - in reality we'd need to check state
        self.handshake_complete
    }

    /// Perform BitTorrent handshake over uTP
    pub async fn perform_handshake(&mut self) -> Result<()> {
        use aria2_protocol::bittorrent::message::handshake::Handshake;
        use aria2_protocol::bittorrent::peer::id::generate_peer_id;
        
        // Generate proper peer_id following BEP 20 format (-AR0001- prefix)
        let peer_id = generate_peer_id();
        let handshake = Handshake::new(&self.info_hash, &peer_id);
        let handshake_bytes = handshake.to_bytes();
        
        // Send handshake
        {
            let mut socket = self.socket.lock().await;
            socket.send(self.conn_id, &handshake_bytes)
                .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e.to_string() }))?;
        }
        
        // Receive handshake response
        let mut response_buf = vec![0u8; 68]; // Handshake is 68 bytes
        let len = {
            let mut socket = self.socket.lock().await;
            socket.recv(self.conn_id, &mut response_buf)
                .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e.to_string() }))?
        };
        
        if len < 68 {
            return Err(Aria2Error::Fatal(FatalError::Config("Handshake response too short".to_string())));
        }
        
        // Parse handshake
        let response = Handshake::parse(&response_buf[..len])
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))?;
        
        // Verify info hash
        if response.info_hash != self.info_hash {
            return Err(Aria2Error::Fatal(FatalError::Config("Info hash mismatch".to_string())));
        }
        
        self.handshake_complete = true;
        Ok(())
    }

    /// Send a BitTorrent message
    pub async fn send_message(&mut self, message: &[u8]) -> Result<()> {
        let mut socket = self.socket.lock().await;
        socket.send(self.conn_id, message)
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e.to_string() }))?;
        Ok(())
    }

    /// Receive a BitTorrent message
    pub async fn recv_message(&mut self) -> Result<Option<Vec<u8>>> {
        // Try to receive data
        let mut buf = vec![0u8; 4096];
        let len = {
            let mut socket = self.socket.lock().await;
            match socket.recv(self.conn_id, &mut buf) {
                Ok(0) => return Ok(None), // No data available
                Ok(len) => len,
                Err(e) => return Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e.to_string() })),
            }
        };
        
        // Append to receive buffer
        self.recv_buffer.extend_from_slice(&buf[..len]);
        
        // Try to parse a complete message
        if self.recv_buffer.len() >= 4 {
            // Read message length (4 bytes, big-endian)
            let msg_len = u32::from_be_bytes([
                self.recv_buffer[0],
                self.recv_buffer[1],
                self.recv_buffer[2],
                self.recv_buffer[3],
            ]) as usize;
            
            // Check if we have the complete message
            if self.recv_buffer.len() >= 4 + msg_len {
                // Extract message
                let message = self.recv_buffer[4..4 + msg_len].to_vec();
                self.recv_buffer = self.recv_buffer[4 + msg_len..].to_vec();
                return Ok(Some(message));
            }
        }
        
        Ok(None)
    }

    /// Close the connection
    pub async fn close(&mut self) -> Result<()> {
        let mut socket = self.socket.lock().await;
        socket.close_connection(self.conn_id)
            .map_err(|e| Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e.to_string() }))?;
        Ok(())
    }

    /// Get connection statistics
    pub async fn stats(&self) -> Result<aria2_protocol::bittorrent::utp::ConnectionStats> {
        let socket = self.socket.lock().await;
        socket.connection_stats(self.conn_id)
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))
    }
}

/// Peer connection abstraction that supports both plain and encrypted (MSE) connections.
///
/// This mirrors the original aria2 C++ architecture where connection management
/// is separated from the download command logic (see BtRuntime in original).
/// Now also supports uTP (UDP-based) connections per BEP 29.
pub struct BtPeerConn {
    pub(crate) inner: InnerConnection,
    /// Set of piece indices for which the peer has sent an AllowedFast message.
    /// Pieces in this set can be requested even when the peer is choked.
    pub allowed_fast: HashSet<u32>,
    /// Connection type (TCP or uTP)
    pub connection_type: ConnectionType,
}

/// Type of peer connection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    /// Standard TCP connection
    Tcp,
    /// uTP (UDP-based) connection
    Utp,
}

impl BtPeerConn {
    /// Connect via MSE (Message Stream Encryption) over TCP
    pub async fn connect_mse(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
        require_encryption: bool,
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::encrypted_connection::EncryptedConnection::connect_with_mse(addr, info_hash, require_encryption).await {
            Ok(conn) => Ok(Self {
                inner: InnerConnection::Encrypted(conn),
                allowed_fast: HashSet::new(),
                connection_type: ConnectionType::Tcp,
            }),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Connect via plain TCP
    pub async fn connect_plain(
        addr: &aria2_protocol::bittorrent::peer::connection::PeerAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        match aria2_protocol::bittorrent::peer::connection::PeerConnection::connect(addr, info_hash)
            .await
        {
            Ok(conn) => Ok(Self {
                inner: InnerConnection::Plain(conn),
                allowed_fast: HashSet::new(),
                connection_type: ConnectionType::Tcp,
            }),
            Err(e) => Err(Aria2Error::Fatal(FatalError::Config(e))),
        }
    }

    /// Connect via uTP (Micro Transport Protocol)
    ///
    /// uTP is a UDP-based transport protocol that provides:
    /// - Reliable, ordered delivery
    /// - LEDBAT congestion control (low priority, background traffic)
    /// - Better performance on congested networks
    /// - NAT traversal benefits
    pub async fn connect_utp(
        addr: std::net::SocketAddr,
        info_hash: &[u8; 20],
    ) -> Result<Self> {
        let utp_conn = UtpPeerConnection::connect(addr, info_hash).await?;
        
        Ok(Self {
            inner: InnerConnection::Utp(utp_conn),
            allowed_fast: HashSet::new(),
            connection_type: ConnectionType::Utp,
        })
    }

    /// Create a uTP connection from an existing socket
    ///
    /// Used when accepting incoming uTP connections
    pub fn from_utp_socket(
        socket: Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>,
        conn_id: u16,
        info_hash: &[u8; 20],
    ) -> Self {
        let utp_conn = UtpPeerConnection::new(socket, conn_id, *info_hash);
        
        Self {
            inner: InnerConnection::Utp(utp_conn),
            allowed_fast: HashSet::new(),
            connection_type: ConnectionType::Utp,
        }
    }

    /// Get the connection type
    pub fn connection_type(&self) -> ConnectionType {
        self.connection_type
    }

    /// Check if this is a uTP connection
    pub fn is_utp(&self) -> bool {
        self.connection_type == ConnectionType::Utp
    }

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

    /// Get a reference to the full AllowedFast set
    ///
    /// Returns all piece indices that this peer has allowed us to request
    /// via BEP 6 Fast Extension, even when choked.
    pub fn allowed_fast_set(&self) -> &HashSet<u32> {
        &self.allowed_fast
    }

    pub async fn send_unchoke(&mut self) -> Result<()> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.send_unchoke().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                // uTP uses the same BitTorrent protocol messages
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

    pub async fn read_message(
        &mut self,
    ) -> Result<Option<aria2_protocol::bittorrent::message::types::BtMessage>> {
        match &mut self.inner {
            InnerConnection::Plain(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Encrypted(c) => c.read_message().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message: e })
            }),
            InnerConnection::Utp(c) => {
                // Receive raw message bytes
                let msg_bytes = c.recv_message().await?;
                if let Some(bytes) = msg_bytes {
                    // Parse into BtMessage
                    use aria2_protocol::bittorrent::message::factory::parse_message;
                    parse_message(&bytes)
                        .map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))
                } else {
                    Ok(None)
                }
            }
        }
    }

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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
