//! uTP peer connection wrapper.
//!
//! Wraps a uTP stream for BitTorrent peer communication.
//! Provides the same interface as TCP connections but uses UDP-based uTP protocol.

use std::sync::Arc;

use aria2_protocol::bittorrent::utp::{ConnectionState, UtpSocketError};
use bytes::BytesMut;
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
    info_hash_v2: Option<[u8; 32]>,
    /// Whether handshake is complete
    handshake_complete: bool,
    /// Remote peer ID learned during the handshake
    remote_peer_id: Option<[u8; 20]>,
    /// Whether the remote BitTorrent handshake advertised BEP 5 DHT support.
    remote_supports_dht: bool,
    /// Whether the remote BitTorrent handshake advertised BEP 6 support.
    remote_supports_fast_extension: bool,
    remote_endpoint: Option<std::net::SocketAddr>,
    /// Receive buffer for partial messages
    recv_buffer: BytesMut,
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
            info_hash_v2: None,
            handshake_complete: false,
            remote_peer_id: None,
            remote_supports_dht: false,
            remote_supports_fast_extension: false,
            remote_endpoint: None,
            recv_buffer: BytesMut::new(),
        }
    }

    /// Connect and complete the BitTorrent handshake over uTP.
    pub async fn connect_with_options(
        addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        listen_port: Option<u16>,
        dht_enabled: bool,
    ) -> Result<Self> {
        let socket = match listen_port {
            Some(port) => aria2_protocol::bittorrent::utp::UtpSocket::bind_port(port),
            None => aria2_protocol::bittorrent::utp::UtpSocket::bind_any(),
        }
        .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))?;

        Self::connect_with_shared_socket_hybrid(
            Arc::new(Mutex::new(socket)),
            addr,
            info_hash_v1,
            info_hash_v2,
            local_peer_id,
            timeout,
            dht_enabled,
        )
        .await
    }

    /// Connect on uTP while advertising and accepting the BEP 52 hybrid hash.
    pub async fn connect_with_shared_socket_hybrid(
        socket: Arc<Mutex<aria2_protocol::bittorrent::utp::UtpSocket>>,
        addr: std::net::SocketAddr,
        info_hash_v1: &[u8; 20],
        info_hash_v2: Option<&[u8; 32]>,
        local_peer_id: &[u8; 20],
        timeout: std::time::Duration,
        dht_enabled: bool,
    ) -> Result<Self> {
        let conn_id = {
            let mut sock = socket.lock().await;
            sock.connect(addr)
                .map_err(|e| Aria2Error::Fatal(FatalError::Config(e.to_string())))?
        };

        let mut connection = Self {
            socket,
            conn_id,
            info_hash: *info_hash_v1,
            info_hash_v2: info_hash_v2.copied(),
            handshake_complete: false,
            remote_peer_id: None,
            remote_supports_dht: false,
            remote_supports_fast_extension: false,
            remote_endpoint: Some(addr),
            recv_buffer: BytesMut::new(),
        };

        connection.wait_until_established(timeout).await?;
        tokio::time::timeout(
            timeout,
            connection.perform_handshake(local_peer_id, dht_enabled),
        )
        .await
        .map_err(|_| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "uTP BitTorrent handshake timed out".to_string(),
            })
        })??;
        Ok(connection)
    }

    /// Return the remote peer ID learned during the handshake.
    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        self.remote_peer_id
    }

    /// Whether the remote BitTorrent handshake advertised BEP 5 DHT support.
    pub fn remote_supports_dht(&self) -> bool {
        self.remote_supports_dht
    }

    /// Whether the remote BitTorrent handshake advertised BEP 6 support.
    pub fn remote_supports_fast_extension(&self) -> bool {
        self.remote_supports_fast_extension
    }

    pub fn remote_addr(&self) -> Option<std::net::SocketAddr> {
        self.remote_endpoint
    }

    /// Get the connection ID.
    pub fn conn_id(&self) -> u16 {
        self.conn_id
    }

    /// Check if connection is established.
    pub fn is_connected(&self) -> bool {
        self.handshake_complete
    }

    async fn wait_until_established(&self, timeout: std::time::Duration) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let readiness = {
                let mut socket = self.socket.lock().await;
                let mut scratch = [];
                let _ = socket.recv(self.conn_id, &mut scratch).map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e.to_string(),
                    })
                })?;
                match socket.connection_state(self.conn_id) {
                    Ok(ConnectionState::Established) => return Ok(()),
                    Ok(
                        ConnectionState::Closed
                        | ConnectionState::Closing
                        | ConnectionState::FinWait
                        | ConnectionState::TimeWait,
                    ) => {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: "uTP connection closed during setup".to_string(),
                            },
                        ));
                    }
                    Ok(_) => socket.readiness_socket().map_err(|e| {
                        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                            message: e.to_string(),
                        })
                    })?,
                    Err(e) => {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: e.to_string(),
                            },
                        ));
                    }
                }
            };

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: "uTP connection setup timed out".to_string(),
                    },
                ));
            }
            tokio::time::timeout(remaining, readiness.readable())
                .await
                .map_err(|_| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: "uTP connection setup timed out".to_string(),
                    })
                })?
                .map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: e.to_string(),
                    })
                })?;
        }
    }

    /// Receive one available uTP payload without holding the socket lock
    /// while waiting for network readiness.
    async fn recv_available(&self, buf: &mut [u8]) -> Result<Option<usize>> {
        loop {
            let readiness = {
                let mut socket = self.socket.lock().await;
                match socket.recv(self.conn_id, buf) {
                    Ok(len) if len > 0 => return Ok(Some(len)),
                    Ok(_) => {
                        let closed = match socket.connection_state(self.conn_id) {
                            Ok(state) => matches!(
                                state,
                                ConnectionState::Closed
                                    | ConnectionState::FinWait
                                    | ConnectionState::Closing
                                    | ConnectionState::TimeWait
                            ),
                            Err(UtpSocketError::ConnectionNotFound(_)) => true,
                            Err(_) => false,
                        };
                        if closed {
                            return Ok(None);
                        }

                        socket.readiness_socket().map_err(|e| {
                            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                                message: e.to_string(),
                            })
                        })?
                    }
                    Err(e) => {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: e.to_string(),
                            },
                        ));
                    }
                }
            };

            readiness.readable().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
        }
    }

    /// Perform the BitTorrent handshake over uTP.
    pub async fn perform_handshake(
        &mut self,
        local_peer_id: &[u8; 20],
        dht_enabled: bool,
    ) -> Result<()> {
        use aria2_protocol::bittorrent::message::handshake::Handshake;

        let handshake = Handshake::new(&self.info_hash, local_peer_id)
            .with_dht(dht_enabled)
            .with_bep52(self.info_hash_v2.is_some());
        let handshake_bytes = handshake.to_bytes();

        {
            let mut socket = self.socket.lock().await;
            socket.send(self.conn_id, &handshake_bytes).map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
        }

        while self.recv_buffer.len() < 68 {
            let mut response_buf = vec![0u8; constants::BT_RECEIVE_BUFFER_SIZE];
            let len = self
                .recv_available(&mut response_buf)
                .await?
                .ok_or_else(|| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: "uTP connection closed during handshake".to_string(),
                    })
                })?;
            self.recv_buffer.extend_from_slice(&response_buf[..len]);
        }

        let response = Handshake::parse(&self.recv_buffer.split_to(68))
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(e)))?;

        let accepted = response.info_hash == self.info_hash
            || self
                .info_hash_v2
                .is_some_and(|hash| response.info_hash == hash[..20] && response.supports_bep52());
        if !accepted {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Info hash mismatch".to_string(),
            )));
        }

        self.remote_peer_id = Some(response.peer_id);
        self.remote_supports_dht = response.supports_dht();
        self.remote_supports_fast_extension = response.supports_fast_extension();
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
        loop {
            if self.recv_buffer.len() >= 4 {
                let msg_len =
                    u32::from_be_bytes(self.recv_buffer[..4].try_into().unwrap()) as usize;
                let frame_len = msg_len.checked_add(4).ok_or_else(|| {
                    Aria2Error::Fatal(FatalError::Config(
                        "uTP BitTorrent message length overflows address space".to_string(),
                    ))
                })?;

                if self.recv_buffer.len() >= frame_len {
                    return Ok(Some(self.recv_buffer.split_to(frame_len).to_vec()));
                }
            }

            let mut buf = vec![0u8; constants::BT_RECEIVE_BUFFER_SIZE];
            match self.recv_available(&mut buf).await? {
                Some(len) => self.recv_buffer.extend_from_slice(&buf[..len]),
                None if self.recv_buffer.is_empty() => return Ok(None),
                None => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: "uTP connection closed with an incomplete BitTorrent message"
                                .to_string(),
                        },
                    ));
                }
            }
        }
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
