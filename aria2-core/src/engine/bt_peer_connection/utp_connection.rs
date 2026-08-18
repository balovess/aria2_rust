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
    /// Whether handshake is complete
    handshake_complete: bool,
    /// Remote peer ID learned during the handshake
    remote_peer_id: Option<[u8; 20]>,
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
            handshake_complete: false,
            remote_peer_id: None,
            remote_endpoint: None,
            recv_buffer: BytesMut::new(),
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
            remote_peer_id: None,
            remote_endpoint: Some(addr),
            recv_buffer: BytesMut::new(),
        })
    }

    /// Return the remote peer ID learned during the handshake.
    pub fn remote_peer_id(&self) -> Option<[u8; 20]> {
        self.remote_peer_id
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
        let len = self
            .recv_available(&mut response_buf)
            .await?
            .ok_or_else(|| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: "uTP connection closed during handshake".to_string(),
                })
            })?;

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

        self.remote_peer_id = Some(response.peer_id);
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

#[cfg(test)]
mod tests {
    use super::UtpPeerConnection;
    use aria2_protocol::bittorrent::utp::{ConnectionState, UtpSocket};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn recv_message_waits_for_fragmented_frame() {
        let info_hash = [7u8; 20];
        let mut client = UtpSocket::bind("127.0.0.1:0").unwrap();
        let mut server = UtpSocket::bind("127.0.0.1:0").unwrap();
        let client_conn_id = client.connect(server.local_addr()).unwrap();

        for _ in 0..100 {
            server.poll_recv().unwrap();
            client.poll_recv().unwrap();
            if client.connection_state(client_conn_id).unwrap() == ConnectionState::Established {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        assert_eq!(
            client.connection_state(client_conn_id).unwrap(),
            ConnectionState::Established
        );
        let server_conn_id = server.connection_ids()[0];
        let client = Arc::new(Mutex::new(client));
        let server = Arc::new(Mutex::new(server));

        let mut peer = UtpPeerConnection::new(client, client_conn_id, info_hash);
        server
            .lock()
            .await
            .send(server_conn_id, &[0, 0, 0, 1])
            .unwrap();

        let receive = tokio::spawn(async move { peer.recv_message().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        server.lock().await.send(server_conn_id, &[0]).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(1), receive)
            .await
            .expect("fragmented uTP message should complete")
            .expect("receiver task should not panic")
            .expect("receiver should succeed");
        assert_eq!(result, Some(vec![0, 0, 0, 1, 0]));
    }
}
