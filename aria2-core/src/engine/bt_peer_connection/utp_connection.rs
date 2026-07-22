//! uTP peer connection wrapper.
//!
//! Wraps a uTP stream for BitTorrent peer communication.
//! Provides the same interface as TCP connections but uses UDP-based uTP protocol.

use std::sync::Arc;

use tokio::sync::Mutex;

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};

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
