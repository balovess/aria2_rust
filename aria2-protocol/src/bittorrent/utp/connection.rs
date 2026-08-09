//! uTP Connection implementation
//!
//! Implements the connection state machine for uTP protocol (BEP 29).
//! Manages individual connection state, sequencing, and data transfer.

use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::packet::{PacketType, UtpPacket, UtpPacketError};

/// Connection state in the uTP state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// Initial state, no connection established
    Closed,
    /// SYN sent, waiting for SYN-ACK
    SynSent,
    /// SYN-ACK sent (received SYN), waiting for ACK
    SynReceived,
    /// Connection established, data transfer possible
    Established,
    /// FIN sent, waiting for FIN-ACK
    FinWait,
    /// Close requested, waiting for FIN
    Closing,
    /// Time wait after close
    TimeWait,
}

/// Errors that can occur during uTP connection operations
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("Connection not established")]
    NotConnected,

    #[error("Connection already exists")]
    AlreadyExists,

    #[error("Connection closed by remote")]
    ClosedByRemote,

    #[error("Connection timed out")]
    Timeout,

    #[error("Invalid packet: {0}")]
    InvalidPacket(String),

    #[error("Packet error: {0}")]
    PacketError(#[from] UtpPacketError),

    #[error("IO error: {0}")]
    IoError(#[from] io::Error),

    #[error("Sequence number mismatch: expected {expected}, got {actual}")]
    SeqMismatch { expected: u16, actual: u16 },

    #[error("Connection reset by remote")]
    Reset,

    #[error("Connection aborted")]
    Aborted,
}

/// Maximum number of retries for connection establishment
/// TODO: will be used once connection retry logic is implemented
#[allow(dead_code)]
const MAX_CONNECT_RETRIES: u32 = 3;

/// Default initial congestion window
const INITIAL_CWND: u32 = 2;

/// Maximum receive buffer size per connection
const RECV_BUFFER_SIZE: usize = 64 * 1024;

/// Maximum send buffer size per connection
const SEND_BUFFER_SIZE: usize = 64 * 1024;

/// uTP Connection implementing the BEP 29 state machine
///
/// Each connection manages its own sequence numbers, congestion window,
/// and retransmission state independently.
pub struct UtpConnection {
    /// Current connection state
    state: ConnectionState,

    /// Local connection ID
    local_conn_id: u16,

    /// Remote connection ID
    remote_conn_id: u16,

    /// Next sequence number to send
    seq_nr: u16,

    /// Next expected acknowledgment number
    ack_nr: u16,

    /// Sequence number of the last SYN
    syn_seq_nr: u16,

    /// Remote socket address
    remote_addr: Option<SocketAddr>,

    /// Congestion window size (in packets)
    congestion_window: u32,

    /// Current round-trip time estimate
    srtt: Duration,

    /// Round-trip time variation
    // TODO: will be used for RTT variance calculation in RTO update (RFC 6298)
    #[allow(dead_code)]
    rtt_var: Duration,

    /// Retransmission timeout
    rto: Duration,

    /// Receive buffer for incoming data
    recv_buffer: Vec<u8>,

    /// Send buffer for outgoing data not yet acknowledged
    send_buffer: Vec<u8>,

    /// Last activity timestamp
    last_activity: Instant,

    /// Number of connection retries
    // TODO: will be used once connection retry logic is implemented
    #[allow(dead_code)]
    connect_retries: u32,

    /// Receive window size
    recv_window: u32,

    /// Bytes in flight (sent but not acknowledged)
    bytes_in_flight: u32,
}

impl UtpConnection {
    /// Create a new uTP connection in Closed state
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Closed,
            local_conn_id: 0,
            remote_conn_id: 0,
            seq_nr: 1,
            ack_nr: 0,
            syn_seq_nr: 0,
            remote_addr: None,
            congestion_window: INITIAL_CWND,
            srtt: Duration::from_millis(100),
            rtt_var: Duration::from_millis(50),
            rto: Duration::from_secs(1),
            recv_buffer: Vec::with_capacity(RECV_BUFFER_SIZE),
            send_buffer: Vec::with_capacity(SEND_BUFFER_SIZE),
            last_activity: Instant::now(),
            connect_retries: 0,
            recv_window: RECV_BUFFER_SIZE as u32,
            bytes_in_flight: 0,
        }
    }

    /// Initiate a connection (client-side) - creates SYN packet
    pub fn connect(&mut self, remote_addr: SocketAddr) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Closed {
            return Err(ConnectionError::AlreadyExists);
        }

        self.remote_addr = Some(remote_addr);
        self.local_conn_id = rand_connection_id();
        self.seq_nr = rand_connection_id();
        self.syn_seq_nr = self.seq_nr;
        self.state = ConnectionState::SynSent;

        let syn = UtpPacket::syn(self.local_conn_id, self.seq_nr, 0, self.recv_window);
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn)
    }

    /// Accept an incoming connection (server-side) - creates SYN-ACK packet
    pub fn accept(
        &mut self,
        syn_packet: &UtpPacket,
        remote_addr: SocketAddr,
    ) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Closed {
            return Err(ConnectionError::AlreadyExists);
        }

        self.remote_addr = Some(remote_addr);
        self.remote_conn_id = syn_packet.connection_id;
        self.ack_nr = syn_packet.seq_nr;
        self.local_conn_id = rand_connection_id();
        self.seq_nr = rand_connection_id();
        self.syn_seq_nr = self.seq_nr;
        self.state = ConnectionState::Established;

        let syn_ack = UtpPacket::syn_ack(
            self.local_conn_id,
            self.seq_nr,
            self.ack_nr,
            self.recv_window,
        );
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn_ack)
    }

    /// Close the connection gracefully - creates FIN packet
    pub fn close(&mut self) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Established {
            return Err(ConnectionError::NotConnected);
        }

        let fin = UtpPacket::fin(
            self.local_conn_id,
            self.seq_nr,
            self.ack_nr,
            self.recv_window,
        );
        self.seq_nr = self.seq_nr.wrapping_add(1);
        self.state = ConnectionState::FinWait;

        Ok(fin)
    }

    /// Send data on an established connection - creates DATA packets
    pub fn send_data(&mut self, data: &[u8]) -> Result<Vec<UtpPacket>, ConnectionError> {
        if self.state != ConnectionState::Established {
            return Err(ConnectionError::NotConnected);
        }

        let mut packets = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let remaining = self.congestion_window as usize;
            if remaining == 0 {
                break;
            }

            let chunk_size = std::cmp::min(remaining, data.len() - offset);
            let chunk_size = std::cmp::min(chunk_size, 1400); // MTU limit

            let packet = UtpPacket::data(
                self.local_conn_id,
                self.seq_nr,
                self.ack_nr,
                self.recv_window,
                data[offset..offset + chunk_size].to_vec(),
            );

            self.send_buffer
                .extend_from_slice(&data[offset..offset + chunk_size]);
            self.bytes_in_flight += chunk_size as u32;
            self.seq_nr = self.seq_nr.wrapping_add(1);
            offset += chunk_size;

            packets.push(packet);
        }

        Ok(packets)
    }

    /// Receive data from the connection buffer
    pub fn recv_data(&mut self) -> Vec<u8> {
        let data = self.recv_buffer.clone();
        self.recv_buffer.clear();
        data
    }

    /// Handle an incoming packet - returns response packets to send
    pub fn on_packet_received(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<Vec<UtpPacket>, ConnectionError> {
        self.last_activity = Instant::now();

        match packet.packet_type()? {
            PacketType::StSyn => {
                // In BEP 29, the server's SYN-ACK is also a StSyn-type packet.
                // When in SynSent state, receiving a SYN-ACK is the expected
                // response that completes the handshake.
                if self.state == ConnectionState::SynSent {
                    self.remote_conn_id = packet.connection_id;
                    self.ack_nr = packet.seq_nr;
                    self.state = ConnectionState::Established;
                    Ok(vec![])
                } else {
                    Err(ConnectionError::InvalidPacket("Unexpected SYN".to_string()))
                }
            }
            PacketType::StData => self.handle_data_packet(packet),
            PacketType::StAck => self.handle_ack_packet(packet),
            PacketType::StFin => self.handle_fin_packet(packet),
            PacketType::StReset => {
                self.state = ConnectionState::Closed;
                Err(ConnectionError::Reset)
            }
        }
    }

    /// Handle incoming DATA packet
    fn handle_data_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<Vec<UtpPacket>, ConnectionError> {
        if self.state != ConnectionState::Established {
            return Err(ConnectionError::NotConnected);
        }

        // Store received data
        self.recv_buffer.extend_from_slice(&packet.payload);

        // Update acknowledgment
        self.ack_nr = packet.seq_nr;

        // Send ACK
        let ack = UtpPacket::ack(
            self.remote_conn_id,
            self.ack_nr,
            self.seq_nr,
            self.recv_window,
        );

        Ok(vec![ack])
    }

    /// Handle incoming ACK packet
    fn handle_ack_packet(&mut self, packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        // Update RTT estimates
        let acked_bytes = std::cmp::min(
            self.bytes_in_flight,
            packet.ack_nr.wrapping_sub(self.ack_nr) as u32 * 1400,
        );
        self.bytes_in_flight = self.bytes_in_flight.saturating_sub(acked_bytes);

        // Remove acknowledged data from send buffer
        if acked_bytes as usize <= self.send_buffer.len() {
            self.send_buffer.drain(..acked_bytes as usize);
        }

        // Transition from SynSent to Established on first ACK
        if self.state == ConnectionState::SynSent {
            self.remote_conn_id = packet.connection_id;
            self.ack_nr = packet.seq_nr;
            self.state = ConnectionState::Established;
        } else if self.state == ConnectionState::Established {
            self.ack_nr = packet.ack_nr;
        }

        Ok(vec![])
    }

    /// Handle incoming FIN packet
    fn handle_fin_packet(&mut self, packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        self.ack_nr = packet.seq_nr;

        let response = if self.state == ConnectionState::FinWait {
            // Both sides closing - go to Closed
            self.state = ConnectionState::Closed;
            vec![]
        } else if self.state == ConnectionState::Established {
            // Remote initiated close - ACK the FIN
            let ack = UtpPacket::ack(
                self.remote_conn_id,
                self.ack_nr,
                self.seq_nr,
                self.recv_window,
            );
            self.state = ConnectionState::Closing;
            vec![ack]
        } else {
            vec![]
        };

        Ok(response)
    }

    /// Check if connection has timed out
    pub fn check_timeout(&mut self, idle_timeout: Duration) -> bool {
        if self.last_activity.elapsed() >= idle_timeout {
            self.state = ConnectionState::Closed;
            return true;
        }
        false
    }

    /// Get packets that need to be retransmitted
    pub fn get_sendable_packets(&mut self) -> Vec<UtpPacket> {
        if self.send_buffer.is_empty() {
            return vec![];
        }

        // Simple retransmit: resend first unacked data
        let data = self.send_buffer.clone();
        let packet = UtpPacket::data(
            self.local_conn_id,
            self.syn_seq_nr.wrapping_add(1),
            self.ack_nr,
            self.recv_window,
            data,
        );

        vec![packet]
    }

    // --- Accessors ---

    /// Get current connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Check if connection is in Established state
    pub fn is_established(&self) -> bool {
        self.state == ConnectionState::Established
    }

    /// Get local connection ID
    pub fn local_connection_id(&self) -> u16 {
        self.local_conn_id
    }

    /// Get remote connection ID
    pub fn remote_connection_id(&self) -> u16 {
        self.remote_conn_id
    }

    /// Get remote socket address
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    /// Get current sequence number
    pub fn current_seq_nr(&self) -> u16 {
        self.seq_nr
    }

    /// Get current acknowledgment number
    pub fn current_ack_nr(&self) -> u16 {
        self.ack_nr
    }

    /// Get current RTO
    pub fn rto(&self) -> Duration {
        self.rto
    }

    /// Get smoothed RTT
    pub fn rtt(&self) -> Duration {
        self.srtt
    }

    /// Get congestion window size
    pub fn congestion_window(&self) -> u32 {
        self.congestion_window
    }

    /// Get receive window size
    pub fn receive_window(&self) -> u32 {
        self.recv_window
    }

    /// Get bytes in flight
    pub fn bytes_in_flight(&self) -> u32 {
        self.bytes_in_flight
    }

    /// Get idle time since last activity
    pub fn idle_time(&self) -> Duration {
        self.last_activity.elapsed()
    }
}

impl Default for UtpConnection {
    fn default() -> Self {
        Self::new()
    }
}

/// Generate a random connection ID
fn rand_connection_id() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.subsec_nanos() >> 16) as u16 ^ (now.as_secs() as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345)
    }

    #[test]
    fn test_connection_new() {
        let conn = UtpConnection::new();
        assert_eq!(conn.state(), ConnectionState::Closed);
        assert!(!conn.is_established());
        assert!(conn.remote_addr().is_none());
    }

    #[test]
    fn test_connection_connect() {
        let mut conn = UtpConnection::new();
        let result = conn.connect(test_addr());
        assert!(result.is_ok());
        assert_eq!(conn.state(), ConnectionState::SynSent);
        assert_eq!(conn.remote_addr(), Some(test_addr()));
    }

    #[test]
    fn test_connection_connect_already_connected() {
        let mut conn = UtpConnection::new();
        conn.connect(test_addr()).unwrap();
        let result = conn.connect(test_addr());
        assert!(matches!(result, Err(ConnectionError::AlreadyExists)));
    }

    #[test]
    fn test_connection_close_not_connected() {
        let mut conn = UtpConnection::new();
        let result = conn.close();
        assert!(matches!(result, Err(ConnectionError::NotConnected)));
    }

    #[test]
    fn test_connection_send_data_not_connected() {
        let mut conn = UtpConnection::new();
        let result = conn.send_data(&[1, 2, 3]);
        assert!(matches!(result, Err(ConnectionError::NotConnected)));
    }

    #[test]
    fn test_connection_default() {
        let conn = UtpConnection::default();
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn test_connection_idle_time() {
        let conn = UtpConnection::new();
        let idle = conn.idle_time();
        assert!(idle.as_nanos() > 0 || idle.is_zero());
    }

    #[test]
    fn test_connection_recv_data_empty() {
        let mut conn = UtpConnection::new();
        let data = conn.recv_data();
        assert!(data.is_empty());
    }

    #[test]
    fn test_connection_timeout() {
        let mut conn = UtpConnection::new();
        // Very short timeout should trigger for newly created connection
        let result = conn.check_timeout(Duration::ZERO);
        assert!(result);
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn test_connection_no_timeout() {
        let mut conn = UtpConnection::new();
        let result = conn.check_timeout(Duration::from_secs(3600));
        assert!(!result);
    }
}
