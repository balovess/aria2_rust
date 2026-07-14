//! uTP socket implementation
//!
//! Implements the UDP-based socket for uTP protocol as specified in BEP 29.
//! Manages multiple uTP connections over a single UDP socket.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::bittorrent::utp::connection::{ConnectionError, ConnectionState, UtpConnection};
use crate::bittorrent::utp::packet::{PacketType, UtpPacket, UtpPacketError};
use crate::bittorrent::utp::timer::{TimerManager, TimerType};

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

/// Connection identifier (combination of connection_id and remote address)
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct ConnectionId {
    /// Connection ID from uTP packet
    pub conn_id: u16,
    /// Remote socket address
    pub remote_addr: SocketAddr,
}

impl ConnectionId {
    /// Create a new connection identifier
    pub fn new(conn_id: u16, remote_addr: SocketAddr) -> Self {
        Self {
            conn_id,
            remote_addr,
        }
    }
}

/// uTP socket that manages multiple connections over UDP
///
/// This is the main interface for uTP communication. It wraps a UDP socket
/// and provides connection-oriented semantics with congestion control.
pub struct UtpSocket {
    /// Underlying UDP socket
    socket: UdpSocket,
    /// Active connections indexed by connection ID
    connections: HashMap<u16, UtpConnection>,
    /// Mapping from remote address to connection ID (for incoming packets)
    addr_to_conn: HashMap<SocketAddr, u16>,
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
        let socket = UdpSocket::bind(addr).map_err(UtpSocketError::BindFailed)?;
        let local_addr = socket.local_addr().map_err(UtpSocketError::BindFailed)?;
        socket
            .set_nonblocking(true)
            .map_err(UtpSocketError::BindFailed)?;

        Ok(Self {
            socket,
            connections: HashMap::new(),
            addr_to_conn: HashMap::new(),
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

        let mut conn = UtpConnection::new();
        let syn_packet = conn.connect(remote_addr)?;
        let conn_id = conn.local_connection_id();

        self.send_packet(&syn_packet, remote_addr)?;

        self.connections.insert(conn_id, conn);
        self.addr_to_conn.insert(remote_addr, conn_id);

        self.timers
            .set_timer(conn_id, TimerType::ConnectTimeout, self.connect_timeout);

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

        let data = conn.recv_data();
        let len = std::cmp::min(data.len(), buf.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok(len)
    }

    /// Process incoming UDP packets
    fn process_incoming_packets(&mut self) -> Result<(), UtpSocketError> {
        match self.socket.recv_from(&mut self.recv_buffer) {
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
                    let (response_packets, keepalive_interval) = {
                        let conn = self.connections.get_mut(&conn_id);
                        if let Some(conn) = conn {
                            let response_packets = conn.on_packet_received(packet)?;
                            let keepalive_interval = self.keepalive_interval;
                            (response_packets, keepalive_interval)
                        } else {
                            return Ok(());
                        }
                    };

                    for resp_packet in response_packets {
                        self.send_packet(&resp_packet, addr)?;
                    }

                    self.timers.cancel_timer(conn_id, TimerType::ConnectTimeout);
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
        self.addr_to_conn.insert(addr, conn_id);

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
        if let Some(conn_id) = self.addr_to_conn.get(&addr) {
            return Ok(Some(*conn_id));
        }

        for (conn_id, conn) in &self.connections {
            if conn.local_connection_id() == packet.connection_id
                || conn.remote_connection_id() == packet.connection_id
            {
                return Ok(Some(*conn_id));
            }
        }

        Ok(None)
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

        if let Some(addr) = remote_addr {
            self.addr_to_conn.remove(&addr);
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
        self.addr_to_conn.clear();
        self.timers.clear();
    }

    /// Process timers and handle expired ones
    pub fn process_timers(&mut self) -> Result<(), UtpSocketError> {
        let expired = self.timers.get_expired_timers();

        for (conn_id, timer_type) in expired {
            self.handle_timer_expired(conn_id, timer_type)?;
        }

        Ok(())
    }

    /// Handle an expired timer
    fn handle_timer_expired(
        &mut self,
        conn_id: u16,
        timer_type: TimerType,
    ) -> Result<(), UtpSocketError> {
        match timer_type {
            TimerType::ConnectTimeout => {
                let should_close = {
                    let conn = self.connections.get(&conn_id);
                    conn.is_some_and(|c| c.state() == ConnectionState::SynSent)
                };

                if should_close {
                    self.close_connection_internal(conn_id)?;
                }
            }
            TimerType::Retransmit(_) => {
                let (is_timeout, remote_addr, packets_to_send, rto) = {
                    let conn = self.connections.get_mut(&conn_id);
                    if let Some(conn) = conn {
                        let is_timeout = conn.check_timeout(self.idle_timeout);
                        let remote_addr = conn.remote_addr();
                        let packets = conn.get_sendable_packets();
                        let rto = conn.rto();
                        (is_timeout, remote_addr, packets, rto)
                    } else {
                        return Ok(());
                    }
                };

                if is_timeout {
                    self.close_connection_internal(conn_id)?;
                } else {
                    for packet in packets_to_send {
                        if let Some(addr) = remote_addr {
                            self.send_packet(&packet, addr)?;
                            self.timers.set_timer(
                                conn_id,
                                TimerType::Retransmit(packet.seq_nr),
                                rto,
                            );
                        }
                    }
                }
            }
            TimerType::Keepalive => {
                let (
                    is_established,
                    remote_conn_id,
                    ack_nr,
                    seq_nr,
                    recv_window,
                    remote_addr,
                    keepalive_interval,
                ) = {
                    let conn = self.connections.get(&conn_id);
                    if let Some(conn) = conn {
                        let is_established = conn.is_established();
                        let remote_conn_id = conn.remote_connection_id();
                        let ack_nr = conn.current_ack_nr();
                        let seq_nr = conn.current_seq_nr();
                        let recv_window = conn.receive_window();
                        let remote_addr = conn.remote_addr();
                        let keepalive_interval = self.keepalive_interval;
                        (
                            is_established,
                            remote_conn_id,
                            ack_nr,
                            seq_nr,
                            recv_window,
                            remote_addr,
                            keepalive_interval,
                        )
                    } else {
                        return Ok(());
                    }
                };

                if is_established {
                    let ack = UtpPacket::ack(remote_conn_id, ack_nr, seq_nr, recv_window);
                    if let Some(addr) = remote_addr {
                        self.send_packet(&ack, addr)?;
                    }
                    self.timers
                        .set_timer(conn_id, TimerType::Keepalive, keepalive_interval);
                }
            }
            TimerType::IdleTimeout => {
                self.close_connection_internal(conn_id)?;
            }
        }

        Ok(())
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
    /// Local connection ID
    pub local_connection_id: u16,
    /// Remote connection ID
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

/// Async wrapper for UtpSocket
pub struct AsyncUtpSocket {
    inner: Arc<Mutex<UtpSocket>>,
}

impl AsyncUtpSocket {
    /// Create a new async uTP socket
    pub fn bind(addr: &str) -> Result<Self, UtpSocketError> {
        let socket = UtpSocket::bind(addr)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(socket)),
        })
    }

    /// Bind to any available port
    pub fn bind_any() -> Result<Self, UtpSocketError> {
        Self::bind("0.0.0.0:0")
    }

    /// Get local address
    pub async fn local_addr(&self) -> SocketAddr {
        let socket = self.inner.lock().await;
        socket.local_addr()
    }

    /// Connect to a remote peer
    pub async fn connect(&self, remote_addr: SocketAddr) -> Result<u16, UtpSocketError> {
        let mut socket = self.inner.lock().await;
        socket.connect(remote_addr)
    }

    /// Send data on a connection
    pub async fn send(&self, conn_id: u16, data: &[u8]) -> Result<usize, UtpSocketError> {
        let mut socket = self.inner.lock().await;
        socket.send(conn_id, data)
    }

    /// Receive data from a connection
    pub async fn recv(&self, conn_id: u16, buf: &mut [u8]) -> Result<usize, UtpSocketError> {
        let mut socket = self.inner.lock().await;
        socket.recv(conn_id, buf)
    }

    /// Close a connection
    pub async fn close_connection(&self, conn_id: u16) -> Result<(), UtpSocketError> {
        let mut socket = self.inner.lock().await;
        socket.close_connection(conn_id)
    }

    /// Close the socket
    pub async fn close(&self) {
        let mut socket = self.inner.lock().await;
        socket.close();
    }

    /// Get connection state
    pub async fn connection_state(&self, conn_id: u16) -> Result<ConnectionState, UtpSocketError> {
        let socket = self.inner.lock().await;
        socket.connection_state(conn_id)
    }

    /// Get connection stats
    pub async fn connection_stats(&self, conn_id: u16) -> Result<ConnectionStats, UtpSocketError> {
        let socket = self.inner.lock().await;
        socket.connection_stats(conn_id)
    }

    /// Process timers
    pub async fn process_timers(&self) -> Result<(), UtpSocketError> {
        let mut socket = self.inner.lock().await;
        socket.process_timers()
    }

    /// Poll for incoming data
    pub async fn poll_recv(&self) -> Result<Vec<(u16, Vec<u8>)>, UtpSocketError> {
        let mut socket = self.inner.lock().await;
        socket.poll_recv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345)
    }

    #[test]
    fn test_socket_bind() {
        let socket = UtpSocket::bind_any();
        assert!(socket.is_ok());

        let socket = socket.unwrap();
        assert!(socket.local_addr().port() > 0);
        assert!(!socket.is_closed());
        assert_eq!(socket.connection_count(), 0);
    }

    #[test]
    fn test_socket_close() {
        let mut socket = UtpSocket::bind_any().unwrap();
        assert!(!socket.is_closed());

        socket.close();
        assert!(socket.is_closed());
    }

    #[test]
    fn test_socket_connect_closed() {
        let mut socket = UtpSocket::bind_any().unwrap();
        socket.close();

        let result = socket.connect(test_addr());
        assert!(matches!(result, Err(UtpSocketError::SocketClosed)));
    }

    #[test]
    fn test_socket_connection_not_found() {
        let mut socket = UtpSocket::bind_any().unwrap();

        let result = socket.send(60000, &[1, 2, 3]);
        assert!(matches!(
            result,
            Err(UtpSocketError::ConnectionNotFound(60000))
        ));

        let mut buf = [0u8; 100];
        let result = socket.recv(60000, &mut buf);
        assert!(matches!(
            result,
            Err(UtpSocketError::ConnectionNotFound(60000))
        ));
    }

    #[test]
    fn test_connection_id_creation() {
        let conn_id = ConnectionId::new(12345, test_addr());
        assert_eq!(conn_id.conn_id, 12345);
        assert_eq!(conn_id.remote_addr, test_addr());
    }

    #[test]
    fn test_async_socket_bind() {
        let socket = AsyncUtpSocket::bind_any();
        assert!(socket.is_ok());
    }

    #[test]
    fn test_socket_process_timers_no_connections() {
        let mut socket = UtpSocket::bind_any().unwrap();
        let result = socket.process_timers();
        assert!(result.is_ok());
    }

    #[test]
    fn test_socket_poll_recv_no_data() {
        let mut socket = UtpSocket::bind_any().unwrap();
        let result = socket.poll_recv();
        assert!(result.is_ok());
        let data = result.unwrap();
        assert!(data.is_empty());
    }
}
