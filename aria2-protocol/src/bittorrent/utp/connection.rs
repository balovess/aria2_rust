//! uTP Connection implementation
//!
//! Implements the connection state machine for uTP protocol (BEP 29).
//! Manages individual connection state, sequencing, and data transfer.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::congestion::LedbatController;
use super::packet::{PacketType, UtpPacket, UtpPacketError};

mod receive;

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

    /// Connection ID expected on incoming packets.
    local_conn_id: u16,

    /// Connection ID used for outgoing packets after SYN.
    remote_conn_id: u16,

    /// Next sequence number to send
    seq_nr: u16,

    /// Next expected acknowledgment number
    ack_nr: u16,

    /// Sequence number of the last SYN
    syn_seq_nr: u16,

    /// Remote socket address
    remote_addr: Option<SocketAddr>,

    /// Congestion window size in bytes.
    congestion: LedbatController,

    /// Current round-trip time estimate
    srtt: Duration,

    /// Retransmission timeout
    rto: Duration,

    /// Receive buffer for incoming data
    recv_buffer: Vec<u8>,

    /// Send buffer for outgoing data not yet acknowledged
    send_buffer: VecDeque<UtpPacket>,
    pending_receive: HashMap<u16, UtpPacket>,
    pending_receive_bytes: usize,
    accepted_syn_seq: Option<u16>,
    peer_window: u32,

    /// Last activity timestamp
    last_activity: Instant,

    /// Receive window size
    recv_window: u32,
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
            congestion: LedbatController::with_mss(1400),
            srtt: Duration::from_millis(100),
            rto: Duration::from_secs(1),
            recv_buffer: Vec::with_capacity(RECV_BUFFER_SIZE),
            send_buffer: VecDeque::new(),
            pending_receive: HashMap::new(),
            pending_receive_bytes: 0,
            accepted_syn_seq: None,
            peer_window: 0,
            last_activity: Instant::now(),
            recv_window: RECV_BUFFER_SIZE as u32,
        }
    }

    /// Initiate a connection (client-side) - creates SYN packet
    pub fn connect(&mut self, remote_addr: SocketAddr) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Closed {
            return Err(ConnectionError::AlreadyExists);
        }

        self.remote_addr = Some(remote_addr);
        self.local_conn_id = rand_connection_id();
        self.remote_conn_id = self.local_conn_id.wrapping_add(1);
        self.seq_nr = 1;
        self.syn_seq_nr = self.seq_nr;
        self.state = ConnectionState::SynSent;

        let syn = UtpPacket::syn(self.local_conn_id, self.seq_nr, 0, self.recv_window);
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn)
    }

    pub(crate) fn retransmit_packet(&mut self, seq_nr: u16) -> Option<UtpPacket> {
        if self.state == ConnectionState::SynSent && seq_nr == self.syn_seq_nr {
            return Some(UtpPacket::syn(
                self.local_conn_id,
                self.syn_seq_nr,
                0,
                self.recv_window,
            ));
        }

        let packet = self
            .send_buffer
            .iter()
            .find(|packet| packet.seq_nr == seq_nr)
            .cloned()
            .map(|mut packet| {
                packet.ack_nr = self.ack_nr;
                packet.wnd_size = self.recv_window;
                packet
            });
        if packet.is_some() {
            self.congestion.on_loss();
        }
        packet
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

        if syn_packet.packet_type()? != PacketType::StSyn {
            return Err(ConnectionError::InvalidPacket("Expected SYN".to_string()));
        }
        self.remote_addr = Some(remote_addr);
        self.remote_conn_id = syn_packet.connection_id;
        self.accepted_syn_seq = Some(syn_packet.seq_nr);
        self.peer_window = syn_packet.wnd_size;
        self.ack_nr = syn_packet.seq_nr;
        self.local_conn_id = syn_packet.connection_id.wrapping_add(1);
        self.seq_nr = rand_connection_id();
        self.syn_seq_nr = self.seq_nr;
        self.state = ConnectionState::Established;

        let syn_ack = UtpPacket::syn_ack(
            self.remote_conn_id,
            self.seq_nr,
            self.ack_nr,
            self.recv_window,
        );
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn_ack)
    }

    /// Repeat the original STATE response when an accepted SYN is retried.
    pub(crate) fn syn_response(&self, syn: &UtpPacket) -> Option<UtpPacket> {
        (self.remote_conn_id == syn.connection_id
            && self.accepted_syn_seq == Some(syn.seq_nr)
            && self.local_conn_id == syn.connection_id.wrapping_add(1))
        .then(|| {
            UtpPacket::syn_ack(
                self.remote_conn_id,
                self.syn_seq_nr,
                syn.seq_nr,
                self.recv_window,
            )
        })
    }

    /// Close the connection gracefully - creates FIN packet
    pub fn close(&mut self) -> Result<UtpPacket, ConnectionError> {
        if self.state != ConnectionState::Established {
            return Err(ConnectionError::NotConnected);
        }

        let fin = UtpPacket::fin(
            self.remote_conn_id,
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
        let available = self
            .congestion
            .get_window_size()
            .min(self.peer_window)
            .min(SEND_BUFFER_SIZE as u32)
            .saturating_sub(self.congestion.get_bytes_in_flight()) as usize;

        while offset < data.len() {
            let remaining = available.saturating_sub(offset);
            if remaining == 0 {
                break;
            }

            let chunk_size = std::cmp::min(remaining, data.len() - offset);
            let chunk_size = std::cmp::min(chunk_size, 1400); // MTU limit

            let packet = UtpPacket::data(
                self.remote_conn_id,
                self.seq_nr,
                self.ack_nr,
                self.recv_window,
                data[offset..offset + chunk_size].to_vec(),
            );

            self.send_buffer.push_back(packet.clone());
            self.congestion.on_data_sent(chunk_size as u32);
            self.seq_nr = self.seq_nr.wrapping_add(1);
            offset += chunk_size;

            packets.push(packet);
        }

        Ok(packets)
    }

    /// Receive data from the connection buffer
    pub fn recv_data(&mut self) -> Vec<u8> {
        let data = std::mem::take(&mut self.recv_buffer);
        self.update_receive_window();
        data
    }

    /// Read up to the caller's buffer capacity without discarding unread data.
    pub(crate) fn recv_into(&mut self, buffer: &mut [u8]) -> usize {
        let length = buffer.len().min(self.recv_buffer.len());
        buffer[..length].copy_from_slice(&self.recv_buffer[..length]);
        self.recv_buffer.drain(..length);
        self.update_receive_window();
        length
    }

    /// Handle an incoming packet - returns response packets to send
    pub fn on_packet_received(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<Vec<UtpPacket>, ConnectionError> {
        self.on_packet_received_with_acknowledgements(packet)
            .map(|(responses, _)| responses)
    }

    pub(crate) fn on_packet_received_with_acknowledgements(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<(Vec<UtpPacket>, Vec<u16>), ConnectionError> {
        if packet.connection_id != self.local_conn_id {
            return Err(ConnectionError::InvalidPacket(
                "Unexpected connection ID".to_string(),
            ));
        }
        self.last_activity = Instant::now();

        match packet.packet_type()? {
            PacketType::StSyn => Err(ConnectionError::InvalidPacket("Unexpected SYN".to_string())),
            PacketType::StData => self.handle_data_packet(packet),
            PacketType::StAck => self.handle_ack_packet(packet),
            PacketType::StFin => self.handle_fin_packet(packet),
            PacketType::StReset => {
                self.state = ConnectionState::Closed;
                Err(ConnectionError::Reset)
            }
        }
    }

    /// Handle incoming ACK packet
    fn handle_ack_packet(
        &mut self,
        packet: &UtpPacket,
    ) -> Result<(Vec<UtpPacket>, Vec<u16>), ConnectionError> {
        // Transition from SynSent to Established on first ACK
        let acknowledged_syn = (self.state == ConnectionState::SynSent).then_some(self.syn_seq_nr);
        if self.state == ConnectionState::SynSent {
            if packet.ack_nr != self.syn_seq_nr {
                return Err(ConnectionError::InvalidPacket(
                    "SYN was not acknowledged".to_string(),
                ));
            }
            self.ack_nr = packet.seq_nr;
            self.state = ConnectionState::Established;
        }
        let mut acknowledged = self.acknowledge_sent(packet);
        if let Some(seq_nr) = acknowledged_syn {
            acknowledged.push(seq_nr);
        }

        Ok((vec![], acknowledged))
    }

    fn acknowledge_sent(&mut self, packet: &UtpPacket) -> Vec<u16> {
        self.peer_window = packet.wnd_size;
        if packet.ack_nr.wrapping_sub(self.seq_nr.wrapping_sub(1)) as i16 > 0 {
            return Vec::new();
        }
        let mut acknowledged = Vec::new();
        let mut bytes_acknowledged = 0u32;
        while self
            .send_buffer
            .front()
            .is_some_and(|sent| packet.ack_nr.wrapping_sub(sent.seq_nr) < 0x8000)
        {
            let sent = self.send_buffer.pop_front().unwrap();
            bytes_acknowledged = bytes_acknowledged.saturating_add(sent.payload.len() as u32);
            acknowledged.push(sent.seq_nr);
        }
        if bytes_acknowledged > 0 {
            self.congestion.on_ack_received(
                u64::from(packet.timestamp_difference_microseconds),
                bytes_acknowledged,
            );
        }
        acknowledged
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
        self.send_buffer
            .iter()
            .cloned()
            .map(|mut packet| {
                packet.ack_nr = self.ack_nr;
                packet.wnd_size = self.recv_window;
                packet
            })
            .collect()
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

    /// Get the connection ID expected on incoming packets.
    pub fn local_connection_id(&self) -> u16 {
        self.local_conn_id
    }

    /// Get the connection ID used for outgoing packets after SYN.
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
        self.congestion.get_window_size()
    }

    /// Get receive window size
    pub fn receive_window(&self) -> u32 {
        self.recv_window
    }

    /// Get bytes in flight
    pub fn bytes_in_flight(&self) -> u32 {
        self.congestion.get_bytes_in_flight()
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
    rand::random()
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

    #[test]
    fn send_window_tracks_ledbat_ack_growth_and_timeout_loss() {
        let mut conn = UtpConnection::new();
        let syn = conn.connect(test_addr()).unwrap();
        conn.on_packet_received(&UtpPacket::syn_ack(
            syn.connection_id,
            40,
            syn.seq_nr,
            65_536,
        ))
        .unwrap();

        assert_eq!(conn.congestion_window(), 2 * 1400);
        assert_eq!(conn.send_data(&vec![0x33; 2800]).unwrap().len(), 2);
        assert_eq!(conn.bytes_in_flight(), 2800);

        let mut ack = UtpPacket::ack(syn.connection_id, 2, 40, 65_536);
        ack.timestamp_difference_microseconds = 50_000;
        conn.on_packet_received(&ack).unwrap();
        assert_eq!(conn.bytes_in_flight(), 1400);
        assert!(conn.congestion_window() > 2 * 1400);
        assert_eq!(conn.send_data(&vec![0x44; 1400]).unwrap().len(), 1);

        let before_loss = conn.congestion_window();
        let retransmission = conn.retransmit_packet(3).unwrap();
        assert_eq!(retransmission.seq_nr, 3);
        assert!(conn.congestion_window() < before_loss);
        assert_eq!(conn.bytes_in_flight(), 2800);
    }
}
