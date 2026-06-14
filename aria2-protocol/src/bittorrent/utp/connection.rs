//! uTP connection state machine
//!
//! Implements the connection state management for uTP protocol,
//! handling connection establishment, data transfer, and teardown.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::time::Instant;

use crate::bittorrent::utp::congestion::LedbatController;
use crate::bittorrent::utp::metrics::RttEstimator;
use crate::bittorrent::utp::packet::{PacketType, UtpPacket};

/// Connection state for uTP state machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ConnectionState {
    /// Connection is closed
    #[default]
    Closed,
    /// SYN has been sent, waiting for SYN-ACK
    SynSent,
    /// Connection is established and ready for data transfer
    Connected,
    /// FIN has been sent, waiting for all data to be acknowledged
    FinWait,
    /// Connection is in the process of closing
    Closing,
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Closed => write!(f, "CLOSED"),
            ConnectionState::SynSent => write!(f, "SYN_SENT"),
            ConnectionState::Connected => write!(f, "CONNECTED"),
            ConnectionState::FinWait => write!(f, "FIN_WAIT"),
            ConnectionState::Closing => write!(f, "CLOSING"),
        }
    }
}

/// Errors that can occur during connection operations
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error("Invalid state transition: cannot {operation} in state {state}")]
    InvalidStateTransition { state: ConnectionState, operation: String },

    #[error("Connection already exists")]
    AlreadyConnected,

    #[error("Connection not established")]
    NotConnected,

    #[error("Invalid packet: {0}")]
    InvalidPacket(String),

    #[error("Unexpected packet type: {0}")]
    UnexpectedPacketType(PacketType),

    #[error("Connection ID mismatch: expected {expected}, got {actual}")]
    ConnectionIdMismatch { expected: u16, actual: u16 },

    #[error("Sequence number error: {0}")]
    SequenceError(String),

    #[error("Connection reset by peer")]
    ConnectionReset,

    #[error("Connection timeout")]
    Timeout,

    #[error("Send buffer overflow")]
    SendBufferOverflow,

    #[error("Receive buffer overflow")]
    RecvBufferOverflow,
}

/// Pending packet for retransmission
#[derive(Debug, Clone)]
struct PendingPacket {
    /// Packet data (serialized)
    data: Vec<u8>,
    /// Number of transmission attempts
    transmissions: u32,
    /// Time when packet was last sent
    last_sent: Instant,
    /// Whether this packet has been acknowledged
    acknowledged: bool,
}

/// uTP connection state machine
///
/// Manages the complete lifecycle of a uTP connection including
/// connection establishment, data transfer, and graceful shutdown.
#[derive(Debug)]
pub struct UtpConnection {
    /// Current connection state
    state: ConnectionState,
    /// Local connection ID (used for sending)
    local_connection_id: u16,
    /// Remote connection ID (used for receiving)
    remote_connection_id: u16,
    /// Remote socket address
    remote_addr: Option<SocketAddr>,
    /// Next sequence number to send
    seq_nr: u16,
    /// Last acknowledged sequence number
    ack_nr: u16,
    /// Sequence number of the last received packet (for ACK)
    last_recv_seq: u16,
    /// Pending packets awaiting acknowledgment (seq_nr -> PendingPacket)
    send_buffer: HashMap<u16, PendingPacket>,
    /// Ordered queue of sequence numbers for retransmission
    send_queue: VecDeque<u16>,
    /// Received data buffer (seq_nr -> data)
    recv_buffer: HashMap<u16, Vec<u8>>,
    /// Next expected receive sequence number
    expected_recv_seq: u16,
    /// Congestion controller
    congestion_controller: LedbatController,
    /// RTT estimator
    rtt_estimator: RttEstimator,
    /// Time of last activity (used for timeout detection)
    last_activity: Instant,
    /// Time of last packet sent (for keepalive)
    last_send_time: Instant,
    /// Receive window size (advertised to peer)
    recv_window: u32,
    /// Maximum receive buffer size
    max_recv_buffer: usize,
    /// Whether this is an incoming connection (server-side)
    is_incoming: bool,
    /// Connection ID for the SYN packet (used during handshake)
    syn_connection_id: u16,
}

impl Default for UtpConnection {
    fn default() -> Self {
        Self::new()
    }
}

impl UtpConnection {
    /// Default maximum receive buffer size (256 KB)
    const DEFAULT_MAX_RECV_BUFFER: usize = 256 * 1024;

    /// Default receive window size (64 KB)
    const DEFAULT_RECV_WINDOW: u32 = 64 * 1024;

    /// Create a new uTP connection in closed state
    pub fn new() -> Self {
        Self {
            state: ConnectionState::Closed,
            local_connection_id: 0,
            remote_connection_id: 0,
            remote_addr: None,
            seq_nr: 1, // Start at 1 (0 is reserved)
            ack_nr: 0,
            last_recv_seq: 0,
            send_buffer: HashMap::new(),
            send_queue: VecDeque::new(),
            recv_buffer: HashMap::new(),
            expected_recv_seq: 1, // Expect first data at seq 1
            congestion_controller: LedbatController::new(),
            rtt_estimator: RttEstimator::new(),
            last_activity: Instant::now(),
            last_send_time: Instant::now(),
            recv_window: Self::DEFAULT_RECV_WINDOW,
            max_recv_buffer: Self::DEFAULT_MAX_RECV_BUFFER,
            is_incoming: false,
            syn_connection_id: 0,
        }
    }

    /// Create a new connection with custom buffer sizes
    pub fn with_buffer_sizes(max_recv_buffer: usize, recv_window: u32) -> Self {
        let mut conn = Self::new();
        conn.max_recv_buffer = max_recv_buffer;
        conn.recv_window = recv_window;
        conn
    }

    /// Get the current connection state
    pub fn state(&self) -> ConnectionState {
        self.state
    }

    /// Check if the connection is established
    pub fn is_established(&self) -> bool {
        self.state == ConnectionState::Connected
    }

    /// Check if the connection is closed
    pub fn is_closed(&self) -> bool {
        self.state == ConnectionState::Closed
    }

    /// Get the local connection ID
    pub fn local_connection_id(&self) -> u16 {
        self.local_connection_id
    }

    /// Get the remote connection ID
    pub fn remote_connection_id(&self) -> u16 {
        self.remote_connection_id
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> Option<SocketAddr> {
        self.remote_addr
    }

    /// Get the next sequence number (without incrementing)
    pub fn current_seq_nr(&self) -> u16 {
        self.seq_nr
    }

    /// Get the last acknowledged sequence number
    pub fn current_ack_nr(&self) -> u16 {
        self.ack_nr
    }

    /// Get the current receive window size
    pub fn receive_window(&self) -> u32 {
        // Adjust window based on buffer usage
        let buffer_used: usize = self.recv_buffer.values().map(|v| v.len()).sum();
        let available = self.max_recv_buffer.saturating_sub(buffer_used);
        self.recv_window.min(available as u32)
    }

    /// Get the congestion window size
    pub fn congestion_window(&self) -> u32 {
        self.congestion_controller.get_window_size()
    }

    /// Get the available send window (min of congestion window and receive window)
    pub fn available_send_window(&self) -> u32 {
        self.congestion_controller.available_window().min(self.recv_window)
    }

    /// Get bytes in flight
    pub fn bytes_in_flight(&self) -> u32 {
        self.congestion_controller.get_bytes_in_flight()
    }

    /// Get the RTT estimate
    pub fn rtt(&self) -> std::time::Duration {
        self.rtt_estimator.srtt()
    }

    /// Get the retransmission timeout
    pub fn rto(&self) -> std::time::Duration {
        self.rtt_estimator.rto()
    }

    /// Get time since last activity
    pub fn idle_time(&self) -> std::time::Duration {
        self.last_activity.elapsed()
    }

    /// Initiate a connection (client-side)
    ///
    /// Generates a SYN packet and transitions to SynSent state.
    /// Returns the SYN packet to be sent.
    pub fn connect(&mut self, remote_addr: SocketAddr) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Closed {
            return Err(ConnectionError::InvalidStateTransition {
                state: self.state,
                operation: "connect".to_string(),
            });
        }

        // Generate random connection IDs
        // In a real implementation, these should be cryptographically random
        self.local_connection_id = rand_connection_id();
        // Remote connection ID will be learned from SYN-ACK
        self.remote_connection_id = 0;
        self.remote_addr = Some(remote_addr);
        self.syn_connection_id = self.local_connection_id;

        // Create SYN packet
        let syn_packet = UtpPacket::syn(self.local_connection_id, self.seq_nr);

        // Store SYN for retransmission
        self.store_pending_packet(self.seq_nr, syn_packet.clone());

        // Transition to SynSent
        self.state = ConnectionState::SynSent;
        self.last_activity = Instant::now();
        self.last_send_time = Instant::now();

        Ok(syn_packet)
    }

    /// Accept an incoming connection (server-side)
    ///
    /// Called when a SYN packet is received. Creates a SYN-ACK response
    /// and transitions to Connected state.
    pub fn accept(
        &mut self,
        syn_packet: &UtpPacket,
        remote_addr: SocketAddr,
    ) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Closed {
            return Err(ConnectionError::AlreadyConnected);
        }

        // Validate SYN packet
        if syn_packet.packet_type().map_err(|e| ConnectionError::InvalidPacket(e.to_string()))?
            != PacketType::StSyn
        {
            return Err(ConnectionError::UnexpectedPacketType(
                syn_packet.packet_type().unwrap_or(PacketType::StReset),
            ));
        }

        // Generate our connection ID
        self.local_connection_id = rand_connection_id();
        // Remote connection ID is the one from SYN packet
        self.remote_connection_id = syn_packet.connection_id;
        self.remote_addr = Some(remote_addr);
        self.is_incoming = true;

        // Initialize sequence numbers
        self.seq_nr = 1;
        self.ack_nr = syn_packet.seq_nr;
        self.last_recv_seq = syn_packet.seq_nr;

        // Create SYN-ACK (ACK with our connection ID)
        let syn_ack = UtpPacket::ack(
            self.local_connection_id,
            self.ack_nr,
            self.seq_nr,
            self.receive_window(),
        );

        // Transition to Connected
        self.state = ConnectionState::Connected;
        self.last_activity = Instant::now();

        Ok(syn_ack)
    }

    /// Handle an incoming packet
    ///
    /// Processes the packet according to current state and returns
    /// any packets that should be sent in response.
    pub fn on_packet_received(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<Vec<UtpPacket>, ConnectionError> {
        self.last_activity = Instant::now();

        // Validate packet
        let packet_type = packet
            .packet_type()
            .map_err(|e| ConnectionError::InvalidPacket(e.to_string()))?;

        // Check connection ID (except for SYN packets to a listening socket)
        // In SynSent state, we accept ACK packets (SYN-ACK) with any connection_id
        // as we'll learn the remote connection_id from it
        if self.state != ConnectionState::Closed
            && packet_type != PacketType::StSyn
            && self.state != ConnectionState::SynSent
            && packet.connection_id != self.local_connection_id
        {
            return Err(ConnectionError::ConnectionIdMismatch {
                expected: self.local_connection_id,
                actual: packet.connection_id,
            });
        }

        // Update RTT estimate if we have a valid timestamp
        if packet.timestamp_microseconds > 0 {
            // Calculate delay from timestamp difference
            let delay = packet.timestamp_difference_microseconds;
            // This would be used for LEDBAT congestion control
            let _ = delay; // TODO: Integrate with delay estimator
        }

        match packet_type {
            PacketType::StSyn => self.handle_syn(packet),
            PacketType::StData => self.handle_data(packet),
            PacketType::StAck => self.handle_ack(packet),
            PacketType::StFin => self.handle_fin(packet),
            PacketType::StReset => self.handle_reset(packet),
        }
    }

    /// Handle SYN packet
    fn handle_syn(&mut self, packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        match self.state {
            ConnectionState::Closed => {
                // This should be handled by accept(), but we can handle it here too
                Err(ConnectionError::InvalidStateTransition {
                    state: self.state,
                    operation: "handle_syn".to_string(),
                })
            }
            ConnectionState::SynSent => {
                // We received a SYN-ACK (ACK packet in response to our SYN)
                // Learn the server's connection_id
                self.remote_connection_id = packet.connection_id;
                self.ack_nr = packet.seq_nr;
                self.last_recv_seq = packet.seq_nr;

                // Send ACK to complete handshake (use server's connection_id)
                let ack = UtpPacket::ack(
                    self.remote_connection_id,
                    self.ack_nr,
                    self.seq_nr,
                    self.receive_window(),
                );

                self.state = ConnectionState::Connected;

                Ok(vec![ack])
            }
            ConnectionState::Connected => {
                // Duplicate SYN, ignore
                Ok(vec![])
            }
            ConnectionState::FinWait | ConnectionState::Closing => {
                // Connection is closing, send RESET
                Ok(vec![UtpPacket::reset(self.remote_connection_id)])
            }
        }
    }

    /// Handle DATA packet
    fn handle_data(&mut self, packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        if self.state != ConnectionState::Connected {
            return Err(ConnectionError::NotConnected);
        }

        // Check if this is the expected sequence number
        let seq = packet.seq_nr;

        // Buffer the data
        if !packet.payload.is_empty() {
            // Check buffer overflow
            let buffer_used: usize = self.recv_buffer.values().map(|v| v.len()).sum();
            if buffer_used + packet.payload.len() > self.max_recv_buffer {
                return Err(ConnectionError::RecvBufferOverflow);
            }

            self.recv_buffer.insert(seq, packet.payload.clone());
        }

        // Update last received sequence
        self.last_recv_seq = seq;

        // Update ack_nr to the highest contiguous sequence received
        self.update_ack_nr();

        // Send ACK (use sender's connection_id)
        let ack = UtpPacket::ack(
            self.remote_connection_id,
            self.ack_nr,
            self.seq_nr,
            self.receive_window(),
        );

        Ok(vec![ack])
    }

    /// Handle ACK packet
    fn handle_ack(&mut self, packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        match self.state {
            ConnectionState::SynSent => {
                // SYN-ACK received, complete connection
                self.remote_connection_id = packet.connection_id;
                self.ack_nr = packet.ack_nr;
                self.last_recv_seq = packet.seq_nr;

                // Remove SYN from send buffer (it's been ACKed)
                self.remove_pending_packet(self.seq_nr);

                // Send ACK to complete handshake (use server's connection_id)
                let ack = UtpPacket::ack(
                    self.remote_connection_id,
                    self.ack_nr,
                    self.seq_nr,
                    self.receive_window(),
                );

                self.state = ConnectionState::Connected;
                Ok(vec![ack])
            }
            ConnectionState::Connected | ConnectionState::FinWait => {
                // Process ACK and update congestion window
                let ack_nr = packet.ack_nr;

                // Find all packets with seq_nr <= ack_nr and mark as acknowledged
                let mut acked_bytes = 0u32;
                let mut to_remove = Vec::new();

                for (&seq, pending) in &self.send_buffer {
                    if is_seq_before_or_equal(seq, ack_nr) && !pending.acknowledged {
                        to_remove.push(seq);
                        acked_bytes += pending.data.len() as u32;
                    }
                }

                // Remove acknowledged packets
                for seq in to_remove {
                    self.remove_pending_packet(seq);
                }

                // Update congestion controller
                if acked_bytes > 0 {
                    // Use timestamp difference for LEDBAT
                    let timestamp_diff = packet.timestamp_difference_microseconds as u64;
                    self.congestion_controller.on_ack_received(timestamp_diff, acked_bytes);
                }

                // Update RTT estimate
                if let Some(pending) = self.send_buffer.get(&ack_nr) {
                    let rtt = pending.last_sent.elapsed();
                    self.rtt_estimator.add_sample(rtt.as_micros() as u64);
                }

                // If in FinWait and all data acknowledged, transition to Closed
                if self.state == ConnectionState::FinWait && self.send_buffer.is_empty() {
                    self.state = ConnectionState::Closed;
                }

                Ok(vec![])
            }
            ConnectionState::Closed | ConnectionState::Closing => {
                // Ignore ACKs in these states
                Ok(vec![])
            }
        }
    }

    /// Handle FIN packet
    fn handle_fin(&mut self, packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        match self.state {
            ConnectionState::Connected => {
                // Peer is closing, acknowledge and close
                self.ack_nr = packet.seq_nr;

                let fin_ack = UtpPacket::ack(
                    self.remote_connection_id,
                    self.ack_nr,
                    self.seq_nr,
                    self.receive_window(),
                );

                self.state = ConnectionState::Closed;

                Ok(vec![fin_ack])
            }
            ConnectionState::FinWait => {
                // Both sides closing, acknowledge and close
                self.ack_nr = packet.seq_nr;

                let fin_ack = UtpPacket::ack(
                    self.remote_connection_id,
                    self.ack_nr,
                    self.seq_nr,
                    self.receive_window(),
                );

                self.state = ConnectionState::Closed;

                Ok(vec![fin_ack])
            }
            ConnectionState::Closing => {
                // Already closing, acknowledge
                self.ack_nr = packet.seq_nr;

                let fin_ack = UtpPacket::ack(
                    self.remote_connection_id,
                    self.ack_nr,
                    self.seq_nr,
                    self.receive_window(),
                );

                self.state = ConnectionState::Closed;

                Ok(vec![fin_ack])
            }
            ConnectionState::Closed | ConnectionState::SynSent => {
                // Unexpected FIN, send RESET (use packet's connection_id)
                Ok(vec![UtpPacket::reset(packet.connection_id)])
            }
        }
    }

    /// Handle RESET packet
    fn handle_reset(&mut self, _packet: &UtpPacket) -> Result<Vec<UtpPacket>, ConnectionError> {
        // Immediately close connection
        self.state = ConnectionState::Closed;
        self.send_buffer.clear();
        self.send_queue.clear();
        self.recv_buffer.clear();

        Err(ConnectionError::ConnectionReset)
    }

    /// Send data on the connection
    ///
    /// Queues data for transmission. Returns the packets that should be sent.
    pub fn send_data(&mut self, data: &[u8]) -> Result<Vec<UtpPacket>, ConnectionError> {
        if self.state != ConnectionState::Connected {
            return Err(ConnectionError::NotConnected);
        }

        // Split data into packets (simplified - in reality, use MTU)
        const MAX_PACKET_SIZE: usize = 1400; // Conservative MTU

        let mut packets = Vec::new();
        let mut offset = 0;

        while offset < data.len() {
            let chunk_size = std::cmp::min(MAX_PACKET_SIZE, data.len() - offset);
            let chunk = &data[offset..offset + chunk_size];

            // Check if we can send (within window)
            if self.congestion_controller.get_bytes_in_flight() as usize + chunk_size
                > self.congestion_controller.get_window_size() as usize
            {
                // Window full, stop sending
                break;
            }

            let packet = UtpPacket::data(
                self.remote_connection_id, // Use receiver's connection_id
                self.seq_nr,
                self.ack_nr,
                chunk.to_vec(),
            );

            // Track bytes in flight
            self.congestion_controller.on_data_sent(chunk_size as u32);

            // Store for retransmission
            self.store_pending_packet(self.seq_nr, packet.clone());

            packets.push(packet);
            self.seq_nr = self.seq_nr.wrapping_add(1);
            offset += chunk_size;
        }

        self.last_send_time = Instant::now();
        Ok(packets)
    }

    /// Get packets that can be sent (within window)
    ///
    /// Returns packets that are ready to be transmitted, including
    /// retransmissions for timed-out packets.
    pub fn get_sendable_packets(&mut self) -> Vec<UtpPacket> {
        let mut packets = Vec::new();
        let now = Instant::now();
        let rto = self.rtt_estimator.rto();

        // Check for retransmissions
        for seq_nr in self.send_queue.iter() {
            if let Some(pending) = self.send_buffer.get_mut(seq_nr)
                && !pending.acknowledged
                && now.duration_since(pending.last_sent) >= rto
            {
                // Retransmit
                if let Ok(packet) = UtpPacket::from_bytes(&pending.data) {
                    packets.push(packet);
                    pending.transmissions += 1;
                    pending.last_sent = now;
                }
            }
        }

        packets
    }

    /// Receive data from the connection
    ///
    /// Returns ordered data that has been received.
    pub fn recv_data(&mut self) -> Vec<u8> {
        let mut data = Vec::new();

        // Extract data in order starting from expected_recv_seq
        while let Some(chunk) = self.recv_buffer.remove(&self.expected_recv_seq) {
            data.extend(chunk);
            self.expected_recv_seq = self.expected_recv_seq.wrapping_add(1);
        }

        data
    }

    /// Close the connection gracefully
    ///
    /// Sends a FIN packet and transitions to FinWait state.
    pub fn close(&mut self) -> Result<UtpPacket, ConnectionError> {
        match self.state {
            ConnectionState::Connected => {
                // Create FIN packet (use receiver's connection_id)
                let fin_packet = UtpPacket::fin(self.remote_connection_id, self.seq_nr, self.ack_nr);

                // Store for retransmission
                self.store_pending_packet(self.seq_nr, fin_packet.clone());

                // Transition to FinWait
                self.state = ConnectionState::FinWait;
                self.last_activity = Instant::now();

                Ok(fin_packet)
            }
            ConnectionState::SynSent => {
                // Connection not established, just close
                self.state = ConnectionState::Closed;
                Err(ConnectionError::NotConnected)
            }
            ConnectionState::FinWait | ConnectionState::Closing => {
                // Already closing
                Err(ConnectionError::InvalidStateTransition {
                    state: self.state,
                    operation: "close".to_string(),
                })
            }
            ConnectionState::Closed => {
                // Already closed
                Err(ConnectionError::InvalidStateTransition {
                    state: self.state,
                    operation: "close".to_string(),
                })
            }
        }
    }

    /// Force close the connection (send RESET)
    ///
    /// Immediately aborts the connection.
    pub fn reset(&mut self) -> UtpPacket {
        self.state = ConnectionState::Closed;
        self.send_buffer.clear();
        self.send_queue.clear();
        self.recv_buffer.clear();

        UtpPacket::reset(self.remote_connection_id)
    }

    /// Check for timeout and update state
    ///
    /// Should be called periodically to detect connection timeouts.
    pub fn check_timeout(&mut self, timeout: std::time::Duration) -> bool {
        if self.state == ConnectionState::Closed {
            return true;
        }

        if self.last_activity.elapsed() > timeout {
            self.state = ConnectionState::Closed;
            self.congestion_controller.on_timeout();
            return true;
        }

        false
    }

    /// Store a pending packet for retransmission
    fn store_pending_packet(&mut self, seq_nr: u16, packet: UtpPacket) {
        let pending = PendingPacket {
            data: packet.to_bytes(),
            transmissions: 1,
            last_sent: Instant::now(),
            acknowledged: false,
        };

        self.send_buffer.insert(seq_nr, pending);
        self.send_queue.push_back(seq_nr);
    }

    /// Remove a pending packet
    fn remove_pending_packet(&mut self, seq_nr: u16) {
        self.send_buffer.remove(&seq_nr);
        self.send_queue.retain(|&s| s != seq_nr);
    }

    /// Update ack_nr to highest contiguous sequence received
    fn update_ack_nr(&mut self) {
        // Start from current ack_nr and find the highest contiguous sequence
        let mut next_expected = self.ack_nr.wrapping_add(1);

        while self.recv_buffer.contains_key(&next_expected) {
            self.ack_nr = next_expected;
            next_expected = next_expected.wrapping_add(1);
        }
    }
}

/// Compare sequence numbers (wrapping comparison)
///
/// Returns true if a is before b in sequence number space.
fn is_seq_before(a: u16, b: u16) -> bool {
    let diff = a.wrapping_sub(b);
    diff > 0x8000
}

/// Compare sequence numbers (wrapping comparison)
///
/// Returns true if a is before or equal to b in sequence number space.
fn is_seq_before_or_equal(a: u16, b: u16) -> bool {
    a == b || is_seq_before(a, b)
}

/// Generate a random connection ID
///
/// In a real implementation, this should use a cryptographically secure RNG.
fn rand_connection_id() -> u16 {
    // Simple implementation - in production use proper RNG
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let state = RandomState::new();
    let mut hasher = state.build_hasher();
    hasher.write_u64(Instant::now().elapsed().as_nanos() as u64);
    (hasher.finish() & 0xFFFF) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 12345)
    }

    #[test]
    fn test_connection_state_default() {
        let state = ConnectionState::default();
        assert_eq!(state, ConnectionState::Closed);
    }

    #[test]
    fn test_connection_state_display() {
        assert_eq!(format!("{}", ConnectionState::Closed), "CLOSED");
        assert_eq!(format!("{}", ConnectionState::SynSent), "SYN_SENT");
        assert_eq!(format!("{}", ConnectionState::Connected), "CONNECTED");
        assert_eq!(format!("{}", ConnectionState::FinWait), "FIN_WAIT");
        assert_eq!(format!("{}", ConnectionState::Closing), "CLOSING");
    }

    #[test]
    fn test_connection_new() {
        let conn = UtpConnection::new();
        assert_eq!(conn.state(), ConnectionState::Closed);
        assert!(!conn.is_established());
        assert!(conn.is_closed());
        assert_eq!(conn.current_seq_nr(), 1);
        assert_eq!(conn.current_ack_nr(), 0);
    }

    #[test]
    fn test_connection_connect() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        let result = conn.connect(addr);
        assert!(result.is_ok());

        let packet = result.unwrap();
        assert_eq!(packet.packet_type().unwrap(), PacketType::StSyn);
        assert_eq!(conn.state(), ConnectionState::SynSent);
        assert!(!conn.is_established());
        assert_eq!(conn.remote_addr(), Some(addr));
    }

    #[test]
    fn test_connection_connect_invalid_state() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Connect once
        conn.connect(addr).unwrap();

        // Try to connect again
        let result = conn.connect(addr);
        assert!(matches!(
            result,
            Err(ConnectionError::InvalidStateTransition { .. })
        ));
    }

    #[test]
    fn test_connection_accept() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Create a SYN packet
        let syn = UtpPacket::syn(12345, 1);

        let result = conn.accept(&syn, addr);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.packet_type().unwrap(), PacketType::StAck);
        assert_eq!(conn.state(), ConnectionState::Connected);
        assert!(conn.is_established());
        assert_eq!(conn.remote_addr(), Some(addr));
    }

    #[test]
    fn test_connection_accept_already_connected() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Accept first connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Try to accept another
        let syn2 = UtpPacket::syn(54321, 1);
        let result = conn.accept(&syn2, addr);
        assert!(matches!(result, Err(ConnectionError::AlreadyConnected)));
    }

    #[test]
    fn test_connection_send_data_not_connected() {
        let mut conn = UtpConnection::new();
        let data = vec![1, 2, 3, 4, 5];

        let result = conn.send_data(&data);
        assert!(matches!(result, Err(ConnectionError::NotConnected)));
    }

    #[test]
    fn test_connection_send_data() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Send data
        let data = vec![1, 2, 3, 4, 5];
        let result = conn.send_data(&data);
        assert!(result.is_ok());

        let packets = result.unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].packet_type().unwrap(), PacketType::StData);
        assert_eq!(packets[0].payload, data);
    }

    #[test]
    fn test_connection_send_large_data() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Send data larger than MTU
        let data = vec![0u8; 3000]; // Larger than 1400 byte MTU
        let result = conn.send_data(&data);
        assert!(result.is_ok());

        let packets = result.unwrap();
        assert!(packets.len() >= 2); // Should be split into multiple packets
    }

    #[test]
    fn test_connection_handle_data() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Receive DATA packet
        let data_packet = UtpPacket::data(conn.local_connection_id(), 1, 0, vec![1, 2, 3, 4, 5]);
        let result = conn.on_packet_received(&data_packet);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].packet_type().unwrap(), PacketType::StAck);

        // Check received data
        let received = conn.recv_data();
        assert_eq!(received, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_connection_handle_ack() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        conn.connect(addr).unwrap();

        // Simulate receiving SYN-ACK (must use client's connection_id)
        let syn_ack = UtpPacket::ack(conn.local_connection_id(), 1, 100, 65535);
        let result = conn.on_packet_received(&syn_ack);
        assert!(result.is_ok());
        assert_eq!(conn.state(), ConnectionState::Connected);
    }

    #[test]
    fn test_connection_handle_fin() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Receive FIN
        let fin = UtpPacket::fin(conn.local_connection_id(), 2, 1);
        let result = conn.on_packet_received(&fin);
        assert!(result.is_ok());

        let response = result.unwrap();
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].packet_type().unwrap(), PacketType::StAck);
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn test_connection_handle_reset() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Receive RESET
        let reset = UtpPacket::reset(conn.local_connection_id());
        let result = conn.on_packet_received(&reset);
        assert!(matches!(result, Err(ConnectionError::ConnectionReset)));
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn test_connection_close() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Close connection
        let result = conn.close();
        assert!(result.is_ok());

        let fin = result.unwrap();
        assert_eq!(fin.packet_type().unwrap(), PacketType::StFin);
        assert_eq!(conn.state(), ConnectionState::FinWait);
    }

    #[test]
    fn test_connection_close_not_connected() {
        let mut conn = UtpConnection::new();

        let result = conn.close();
        assert!(matches!(
            result,
            Err(ConnectionError::InvalidStateTransition { .. })
        ));
    }

    #[test]
    fn test_connection_reset() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Force reset
        let reset = conn.reset();
        assert_eq!(reset.packet_type().unwrap(), PacketType::StReset);
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn test_connection_timeout() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Check timeout with very short duration
        let is_timeout = conn.check_timeout(std::time::Duration::from_nanos(1));
        assert!(is_timeout);
        assert_eq!(conn.state(), ConnectionState::Closed);
    }

    #[test]
    fn test_connection_receive_window() {
        let conn = UtpConnection::new();
        assert!(conn.receive_window() > 0);
    }

    #[test]
    fn test_connection_congestion_window() {
        let conn = UtpConnection::new();
        assert!(conn.congestion_window() > 0);
    }

    #[test]
    fn test_seq_before() {
        // Test sequence number wrapping
        assert!(is_seq_before(1, 2));
        assert!(is_seq_before(100, 200));
        assert!(!is_seq_before(200, 100));
        assert!(is_seq_before(65535, 0)); // Wrapping case
        assert!(!is_seq_before(0, 65535));
    }

    #[test]
    fn test_seq_before_or_equal() {
        assert!(is_seq_before_or_equal(1, 1));
        assert!(is_seq_before_or_equal(1, 2));
        assert!(!is_seq_before_or_equal(2, 1));
    }

    #[test]
    fn test_connection_id_mismatch() {
        let mut conn = UtpConnection::new();
        let addr = test_addr();

        // Establish connection
        let syn = UtpPacket::syn(12345, 1);
        conn.accept(&syn, addr).unwrap();

        // Send packet with wrong connection ID
        let wrong_id_packet = UtpPacket::data(59999, 1, 0, vec![1, 2, 3]);
        let result = conn.on_packet_received(&wrong_id_packet);
        assert!(matches!(
            result,
            Err(ConnectionError::ConnectionIdMismatch { .. })
        ));
    }

    #[test]
    fn test_connection_buffer_sizes() {
        let conn = UtpConnection::with_buffer_sizes(1024 * 1024, 128 * 1024);
        assert_eq!(conn.max_recv_buffer, 1024 * 1024);
        assert_eq!(conn.recv_window, 128 * 1024);
    }

    #[test]
    fn test_connection_full_handshake() {
        // Simulate a full connection handshake
        let mut client = UtpConnection::new();
        let mut server = UtpConnection::new();
        let client_addr = test_addr();
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 54321);

        // Client sends SYN
        let syn = client.connect(server_addr).unwrap();
        assert_eq!(client.state(), ConnectionState::SynSent);

        // Server receives SYN and sends SYN-ACK
        let syn_ack = server.accept(&syn, client_addr).unwrap();
        assert_eq!(server.state(), ConnectionState::Connected);

        // Client receives SYN-ACK
        let response = client.on_packet_received(&syn_ack).unwrap();
        assert_eq!(client.state(), ConnectionState::Connected);
        assert_eq!(response.len(), 1);
        assert_eq!(response[0].packet_type().unwrap(), PacketType::StAck);

        // Both sides are now connected
        assert!(client.is_established());
        assert!(server.is_established());
    }

    #[test]
    fn test_connection_data_transfer() {
        let mut client = UtpConnection::new();
        let mut server = UtpConnection::new();
        let client_addr = test_addr();
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 54321);

        // Establish connection
        let syn = client.connect(server_addr).unwrap();
        let syn_ack = server.accept(&syn, client_addr).unwrap();
        client.on_packet_received(&syn_ack).unwrap();

        // Client sends data
        let data = vec![1, 2, 3, 4, 5];
        let data_packets = client.send_data(&data).unwrap();
        assert_eq!(data_packets.len(), 1);

        // Server receives data
        let ack = server.on_packet_received(data_packets.first().unwrap()).unwrap();
        assert_eq!(ack.len(), 1);
        assert_eq!(ack[0].packet_type().unwrap(), PacketType::StAck);

        // Server reads data
        let received = server.recv_data();
        assert_eq!(received, data);
    }

    #[test]
    fn test_connection_graceful_close() {
        let mut client = UtpConnection::new();
        let mut server = UtpConnection::new();
        let client_addr = test_addr();
        let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 54321);

        // Establish connection
        let syn = client.connect(server_addr).unwrap();
        let syn_ack = server.accept(&syn, client_addr).unwrap();
        client.on_packet_received(&syn_ack).unwrap();

        // Client initiates close
        let fin = client.close().unwrap();
        assert_eq!(client.state(), ConnectionState::FinWait);

        // Server receives FIN
        let ack = server.on_packet_received(&fin).unwrap();
        assert_eq!(ack.len(), 1);
        assert_eq!(ack[0].packet_type().unwrap(), PacketType::StAck);
        assert_eq!(server.state(), ConnectionState::Closed);

        // Client receives ACK
        client.on_packet_received(&ack[0]).unwrap();
        // After ACK in FinWait with empty send buffer, connection closes
        // (In this case, send buffer is empty so it should close)
    }
}