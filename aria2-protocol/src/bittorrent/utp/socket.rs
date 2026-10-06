//! uTP socket implementation
//!
//! Implements the UDP-based socket for uTP protocol as specified in BEP 29.
//! Manages multiple uTP connections over a single UDP socket.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::bittorrent::utp::connection::{ConnectionError, ConnectionState, UtpConnection};
use crate::bittorrent::utp::packet::{PacketType, UtpPacket, UtpPacketError};
use crate::bittorrent::utp::timer::{TimerManager, TimerType};

#[cfg(test)]
mod tests;
mod timers;

/// Maximum number of concurrent uTP connections per socket
const MAX_CONNECTIONS: usize = 100;

/// Default timeout for connection establishment
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default timeout for idle connections
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default keepalive interval
const DEFAULT_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Maximum receive buffer size for UDP
const MAX_UDP_RECV_BUFFER: usize = 65535;

/// Errors that can occur during socket operations
#[derive(Debug, thiserror::Error)]
pub enum UtpSocketError {
    #[error("Failed to bind UDP socket: {0}")]
    BindFailed(std::io::Error),

    #[error("Failed to send packet: {0}")]
    SendFailed(std::io::Error),

    #[error("Failed to receive packet: {0}")]
    RecvFailed(std::io::Error),

    #[error("Connection error: {0}")]
    ConnectionError(#[from] ConnectionError),

    #[error("Packet error: {0}")]
    PacketError(#[from] UtpPacketError),

    #[error("Maximum connections reached")]
    MaxConnectionsReached,

    #[error("Connection not found: {0}")]
    ConnectionNotFound(u16),

    #[error("Address not found for connection: {0}")]
    AddressNotFound(u16),

    #[error("Socket closed")]
    SocketClosed,

    #[error("Invalid packet from {addr}: {reason}")]
    InvalidPacket { addr: SocketAddr, reason: String },

    #[error("Timeout: {0}")]
    Timeout(String),
}

/// uTP socket that manages multiple connections over UDP
///
/// This is the main interface for uTP communication. It wraps a UDP socket
/// and provides connection-oriented semantics with congestion control.
pub struct UtpSocket {
    /// Underlying UDP socket
    socket: UdpSocket,
    /// Tokio registration for async read readiness.
    ///
    /// The synchronous socket remains the owner of the protocol state. This
    /// handle is initialized only from an async receive path and shares the
    /// same OS socket, allowing callers to await readiness without polling.
    async_socket: OnceLock<Arc<tokio::net::UdpSocket>>,
    /// Active connections indexed by connection ID
    connections: HashMap<u16, UtpConnection>,
    /// Timer management for all connections
    timers: TimerManager,
    /// Local socket address
    local_addr: SocketAddr,
    /// Connection timeout
    connect_timeout: Duration,
    /// Idle timeout for connections
    idle_timeout: Duration,
    /// Keepalive interval
    keepalive_interval: Duration,
    /// Whether the socket is closed
    is_closed: bool,
    /// Receive buffer
    recv_buffer: Vec<u8>,
}

impl UtpSocket {
    /// Create a new uTP socket bound to the specified address
    pub fn bind(addr: &str) -> Result<Self, UtpSocketError> {
        let address = addr.parse::<SocketAddr>().map_err(|error| {
            UtpSocketError::BindFailed(std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
        })?;
        Self::bind_addr(address)
    }

    /// Create a new uTP socket bound to an explicit local socket address.
    pub fn bind_addr(addr: SocketAddr) -> Result<Self, UtpSocketError> {
        let socket = UdpSocket::bind(addr).map_err(UtpSocketError::BindFailed)?;
        let local_addr = socket.local_addr().map_err(UtpSocketError::BindFailed)?;
        socket
            .set_nonblocking(true)
            .map_err(UtpSocketError::BindFailed)?;

        Ok(Self {
            socket,
            async_socket: OnceLock::new(),
            connections: HashMap::new(),
            timers: TimerManager::new(),
            local_addr,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL,
            is_closed: false,
            recv_buffer: vec![0u8; MAX_UDP_RECV_BUFFER],
        })
    }

    /// Bind to any available port
    pub fn bind_any() -> Result<Self, UtpSocketError> {
        Self::bind("0.0.0.0:0")
    }

    /// Bind to a specific port
    pub fn bind_port(port: u16) -> Result<Self, UtpSocketError> {
        Self::bind(&format!("0.0.0.0:{}", port))
    }

    /// Get the local address this socket is bound to
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return a Tokio handle for waiting on UDP read readiness.
    ///
    /// The handle is created lazily because the synchronous uTP API is also
    /// used by non-async callers and must remain constructible outside a
    /// Tokio runtime. The returned socket is only a readiness/receive view of
    /// the same underlying UDP socket; protocol state still lives here.
    pub fn readiness_socket(&self) -> Result<Arc<tokio::net::UdpSocket>, UtpSocketError> {
        if let Some(socket) = self.async_socket.get() {
            return Ok(Arc::clone(socket));
        }

        let socket = self
            .socket
            .try_clone()
            .map_err(UtpSocketError::RecvFailed)?;
        let async_socket =
            Arc::new(tokio::net::UdpSocket::from_std(socket).map_err(UtpSocketError::RecvFailed)?);
        let _ = self.async_socket.set(Arc::clone(&async_socket));
        Ok(self.async_socket.get().cloned().unwrap_or(async_socket))
    }

    /// Return the time remaining until the next connection timer expires.
    pub fn next_timer_delay(&self) -> Option<Duration> {
        self.timers.next_timer().map(|(_, _, remaining)| remaining)
    }

    /// Set connection timeout
    pub fn set_connect_timeout(&mut self, timeout: Duration) {
        self.connect_timeout = timeout;
    }

    /// Set idle timeout for connections
    pub fn set_idle_timeout(&mut self, timeout: Duration) {
        self.idle_timeout = timeout;
    }

    /// Set keepalive interval
    pub fn set_keepalive_interval(&mut self, interval: Duration) {
        self.keepalive_interval = interval;
    }

    /// Check if socket is closed
    pub fn is_closed(&self) -> bool {
        self.is_closed
    }

    /// Get number of active connections
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// Initiate a uTP connection to a remote peer
    pub fn connect(&mut self, remote_addr: SocketAddr) -> Result<u16, UtpSocketError> {
        if self.is_closed {
            return Err(UtpSocketError::SocketClosed);
        }

        if self.connections.len() >= MAX_CONNECTIONS {
            return Err(UtpSocketError::MaxConnectionsReached);
        }

        let mut attempts = 0;
        let (conn, syn_packet, conn_id) = loop {
            attempts += 1;
            if attempts > MAX_CONNECTIONS + 1 {
                return Err(UtpSocketError::MaxConnectionsReached);
            }
            let mut conn = UtpConnection::new();
            let syn_packet = conn.connect(remote_addr)?;
            let conn_id = conn.local_connection_id();
            if !self.connections.contains_key(&conn_id) {
                break (conn, syn_packet, conn_id);
            }
        };

        self.send_packet(&syn_packet, remote_addr)?;

        let rto = conn.rto();

        self.connections.insert(conn_id, conn);

        self.timers
            .set_timer(conn_id, TimerType::ConnectTimeout, self.connect_timeout);
        self.timers
            .set_timer(conn_id, TimerType::Retransmit(syn_packet.seq_nr), rto);

        Ok(conn_id)
    }

    /// Send a packet to a remote address
    fn send_packet(&self, packet: &UtpPacket, addr: SocketAddr) -> Result<(), UtpSocketError> {
        let data = packet.to_bytes();
        self.socket
            .send_to(&data, addr)
            .map_err(UtpSocketError::SendFailed)?;
        Ok(())
    }

    /// Send data on an established connection
    pub fn send(&mut self, conn_id: u16, data: &[u8]) -> Result<usize, UtpSocketError> {
        if self.is_closed {
            return Err(UtpSocketError::SocketClosed);
        }

        // Get connection info first
        let (remote_addr, rto) = {
            let conn = self
                .connections
                .get(&conn_id)
                .ok_or(UtpSocketError::ConnectionNotFound(conn_id))?;

            if !conn.is_established() {
                return Err(UtpSocketError::ConnectionError(
                    ConnectionError::NotConnected,
                ));
            }

            let remote_addr = conn
                .remote_addr()
                .ok_or(UtpSocketError::AddressNotFound(conn_id))?;
            (remote_addr, conn.rto())
        };

        // Get packets to send
        let packets = {
            let conn = self
                .connections
                .get_mut(&conn_id)
                .ok_or(UtpSocketError::ConnectionNotFound(conn_id))?;
            conn.send_data(data)?
        };

        // Send all packets
        let mut bytes_sent = 0;
        for packet in &packets {
            bytes_sent += packet.payload.len();
            self.send_packet(packet, remote_addr)?;
            self.timers
                .set_timer(conn_id, TimerType::Retransmit(packet.seq_nr), rto);
        }

        Ok(bytes_sent)
    }

    /// Receive data from a connection
    pub fn recv(&mut self, conn_id: u16, buf: &mut [u8]) -> Result<usize, UtpSocketError> {
        if self.is_closed {
            return Err(UtpSocketError::SocketClosed);
        }

        // Process incoming packets first to potentially receive new data
        self.process_incoming_packets()?;

        // Get data from connection
        let conn = self
            .connections
            .get_mut(&conn_id)
            .ok_or(UtpSocketError::ConnectionNotFound(conn_id))?;

        Ok(conn.recv_into(buf))
    }

    /// Process incoming UDP packets
    fn process_incoming_packets(&mut self) -> Result<(), UtpSocketError> {
        let received = if let Some(socket) = self.async_socket.get() {
            socket.try_recv_from(&mut self.recv_buffer)
        } else {
            self.socket.recv_from(&mut self.recv_buffer)
        };

        match received {
            Ok((len, addr)) => {
                let packet = UtpPacket::from_bytes(&self.recv_buffer[..len])?;
                self.handle_packet(&packet, addr)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(UtpSocketError::RecvFailed(e)),
        }
        Ok(())
    }

    /// Handle an incoming packet
    fn handle_packet(
        &mut self,
        packet: &UtpPacket,
        addr: SocketAddr,
    ) -> Result<(), UtpSocketError> {
        let packet_type = packet.packet_type()?;

        match packet_type {
            PacketType::StSyn => self.handle_syn(packet, addr)?,
            PacketType::StData | PacketType::StAck | PacketType::StFin => {
                let conn_id = self.find_connection_for_packet(packet, addr)?;
                if let Some(conn_id) = conn_id {
                    let (response_packets, acknowledged, keepalive_interval) = {
                        let conn = self.connections.get_mut(&conn_id);
                        if let Some(conn) = conn {
                            let (response_packets, acknowledged) =
                                conn.on_packet_received_with_acknowledgements(packet)?;
                            let keepalive_interval = self.keepalive_interval;
                            (response_packets, acknowledged, keepalive_interval)
                        } else {
                            return Ok(());
                        }
                    };

                    for resp_packet in response_packets {
                        self.send_packet(&resp_packet, addr)?;
                    }

                    self.timers.cancel_timer(conn_id, TimerType::ConnectTimeout);
                    for seq_nr in acknowledged {
                        self.timers
                            .cancel_timer(conn_id, TimerType::Retransmit(seq_nr));
                    }
                    self.timers
                        .set_timer(conn_id, TimerType::Keepalive, keepalive_interval);
                }
            }
            PacketType::StReset => {
                let conn_id = self.find_connection_for_packet(packet, addr)?;
                if let Some(conn_id) = conn_id {
                    self.close_connection_internal(conn_id)?;
                }
            }
        }
        Ok(())
    }

    /// Handle incoming SYN packet
    fn handle_syn(&mut self, packet: &UtpPacket, addr: SocketAddr) -> Result<(), UtpSocketError> {
        let receive_id = packet.connection_id.wrapping_add(1);
        if let Some(conn) = self.connections.get(&receive_id) {
            if conn.remote_addr() == Some(addr)
                && let Some(response) = conn.syn_response(packet)
            {
                self.send_packet(&response, addr)?;
            }
            return Ok(());
        }

        if self.connections.len() >= MAX_CONNECTIONS {
            let reset = UtpPacket::reset(packet.connection_id);
            self.send_packet(&reset, addr)?;
            return Err(UtpSocketError::MaxConnectionsReached);
        }

        let mut conn = UtpConnection::new();
        let syn_ack = conn.accept(packet, addr)?;
        let conn_id = conn.local_connection_id();

        self.send_packet(&syn_ack, addr)?;

        self.connections.insert(conn_id, conn);

        self.timers
            .set_timer(conn_id, TimerType::Keepalive, self.keepalive_interval);

        Ok(())
    }

    /// Find the connection ID for an incoming packet
    fn find_connection_for_packet(
        &self,
        packet: &UtpPacket,
        addr: SocketAddr,
    ) -> Result<Option<u16>, UtpSocketError> {
        Ok(self
            .connections
            .get(&packet.connection_id)
            .filter(|conn| conn.remote_addr() == Some(addr))
            .map(|_| packet.connection_id))
    }

    /// Close a connection gracefully
    pub fn close_connection(&mut self, conn_id: u16) -> Result<(), UtpSocketError> {
        if self.is_closed {
            return Err(UtpSocketError::SocketClosed);
        }
        self.close_connection_internal(conn_id)?;
        Ok(())
    }

    /// Internal connection close logic
    fn close_connection_internal(&mut self, conn_id: u16) -> Result<(), UtpSocketError> {
        let (should_send_fin, remote_addr) = {
            let conn = self.connections.get(&conn_id);
            if let Some(conn) = conn {
                (conn.is_established(), conn.remote_addr())
            } else {
                return Ok(());
            }
        };

        if should_send_fin {
            let fin = {
                let conn = self.connections.get_mut(&conn_id);
                if let Some(conn) = conn {
                    conn.close()?
                } else {
                    return Ok(());
                }
            };

            if let Some(addr) = remote_addr {
                self.send_packet(&fin, addr)?;
            }
        }

        self.timers.cancel_all_timers(conn_id);
        self.connections.remove(&conn_id);

        Ok(())
    }

    /// Close the entire socket and all connections
    pub fn close(&mut self) {
        if self.is_closed {
            return;
        }

        self.is_closed = true;

        let conn_ids: Vec<u16> = self.connections.keys().copied().collect();
        for conn_id in conn_ids {
            let _ = self.close_connection_internal(conn_id);
        }

        self.connections.clear();
        self.timers.clear();
    }

    /// Check connection state
    pub fn connection_state(&self, conn_id: u16) -> Result<ConnectionState, UtpSocketError> {
        let conn = self
            .connections
            .get(&conn_id)
            .ok_or(UtpSocketError::ConnectionNotFound(conn_id))?;
        Ok(conn.state())
    }

    /// Get connection statistics
    pub fn connection_stats(&self, conn_id: u16) -> Result<ConnectionStats, UtpSocketError> {
        let conn = self
            .connections
            .get(&conn_id)
            .ok_or(UtpSocketError::ConnectionNotFound(conn_id))?;

        Ok(ConnectionStats {
            state: conn.state(),
            local_connection_id: conn.local_connection_id(),
            remote_connection_id: conn.remote_connection_id(),
            remote_addr: conn.remote_addr(),
            rtt: conn.rtt(),
            rto: conn.rto(),
            congestion_window: conn.congestion_window(),
            receive_window: conn.receive_window(),
            bytes_in_flight: conn.bytes_in_flight(),
            idle_time: conn.idle_time(),
        })
    }

    /// Poll for incoming data (non-blocking)
    pub fn poll_recv(&mut self) -> Result<Vec<(u16, Vec<u8>)>, UtpSocketError> {
        self.process_incoming_packets()?;

        let mut results = Vec::new();
        let conn_ids: Vec<u16> = self.connections.keys().copied().collect();

        for conn_id in conn_ids {
            if let Some(conn) = self.connections.get_mut(&conn_id) {
                let data = conn.recv_data();
                if !data.is_empty() {
                    results.push((conn_id, data));
                }
            }
        }

        Ok(results)
    }

    /// Get list of active connection IDs
    pub fn connection_ids(&self) -> Vec<u16> {
        self.connections.keys().copied().collect()
    }
}

impl Drop for UtpSocket {
    fn drop(&mut self) {
        self.close();
    }
}

/// Connection statistics
#[derive(Debug, Clone)]
pub struct ConnectionStats {
    /// Current connection state
    pub state: ConnectionState,
    /// Connection ID expected on incoming packets.
    pub local_connection_id: u16,
    /// Connection ID used for outgoing packets after SYN.
    pub remote_connection_id: u16,
    /// Remote socket address
    pub remote_addr: Option<SocketAddr>,
    /// Estimated RTT
    pub rtt: Duration,
    /// Retransmission timeout
    pub rto: Duration,
    /// Congestion window size
    pub congestion_window: u32,
    /// Receive window size
    pub receive_window: u32,
    /// Bytes in flight
    pub bytes_in_flight: u32,
    /// Time since last activity
    pub idle_time: Duration,
}
