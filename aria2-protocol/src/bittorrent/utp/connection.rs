//! uTP Connection implementation
//!
//! Implements the BEP 29 uTP connection state machine, sequencing, and data transfer.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::congestion::LedbatController;
use super::metrics::RttEstimator;
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

struct SentPacket {
    packet: UtpPacket,
    sent_at: Instant,
    retransmitted: bool,
}

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

    timestamp_origin: Instant,
    reply_micro: u32,

    /// Sequence number of the last SYN
    syn_seq_nr: u16,
    syn_sent_at: Option<Instant>,
    syn_retransmitted: bool,

    /// Remote socket address
    remote_addr: Option<SocketAddr>,

    /// Congestion window size in bytes.
    congestion: LedbatController,

    rtt_estimator: Option<RttEstimator>,

    /// Receive buffer for incoming data
    recv_buffer: Vec<u8>,

    /// Send buffer for outgoing data not yet acknowledged
    send_buffer: VecDeque<SentPacket>,
    pending_receive: HashMap<u16, UtpPacket>,
    pending_receive_bytes: usize,
    accepted_syn_seq: Option<u16>,
    peer_window: u32,
    local_fin_acked: bool,
    remote_fin_received: bool,

    /// Last activity timestamp
    last_activity: Instant,

    /// Receive window size
    recv_window: u32,
}

impl UtpConnection {
    /// Create a new uTP connection in Closed state
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            state: ConnectionState::Closed,
            local_conn_id: 0,
            remote_conn_id: 0,
            seq_nr: 1,
            ack_nr: 0,
            timestamp_origin: now,
            reply_micro: 0,
            syn_seq_nr: 0,
            syn_sent_at: None,
            syn_retransmitted: false,
            remote_addr: None,
            congestion: LedbatController::with_mss(1400),
            rtt_estimator: None,
            recv_buffer: Vec::with_capacity(RECV_BUFFER_SIZE),
            send_buffer: VecDeque::new(),
            pending_receive: HashMap::new(),
            pending_receive_bytes: 0,
            accepted_syn_seq: None,
            peer_window: 0,
            local_fin_acked: false,
            remote_fin_received: false,
            last_activity: now,
            recv_window: RECV_BUFFER_SIZE as u32,
        }
    }

    fn timestamp_now_microseconds(&self) -> u32 {
        self.timestamp_origin.elapsed().as_micros() as u32
    }

    fn stamp_packet(&self, packet: &mut UtpPacket) {
        packet.timestamp_microseconds = self.timestamp_now_microseconds();
        packet.timestamp_difference_microseconds = self.reply_micro;
    }

    fn update_reply_delay(&mut self, packet: &UtpPacket) {
        self.reply_micro = self
            .timestamp_now_microseconds()
            .wrapping_sub(packet.timestamp_microseconds);
    }

    pub(crate) fn state_packet(&self) -> UtpPacket {
        let mut packet = UtpPacket::ack(
            self.remote_conn_id,
            self.ack_nr,
            self.seq_nr,
            self.recv_window,
        );
        self.stamp_packet(&mut packet);
        packet
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
        let sent_at = Instant::now();
        self.syn_sent_at = Some(sent_at);
        self.syn_retransmitted = false;
        self.state = ConnectionState::SynSent;

        let mut syn = UtpPacket::syn(self.local_conn_id, self.seq_nr, 0, self.recv_window);
        self.stamp_packet(&mut syn);
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn)
    }

    pub(crate) fn retransmit_packet(&mut self, seq_nr: u16) -> Option<UtpPacket> {
        if self.state == ConnectionState::SynSent && seq_nr == self.syn_seq_nr {
            self.syn_retransmitted = true;
            let mut packet =
                UtpPacket::syn(self.local_conn_id, self.syn_seq_nr, 0, self.recv_window);
            self.stamp_packet(&mut packet);
            return Some(packet);
        }

        let packet = self
            .send_buffer
            .iter_mut()
            .find(|sent| sent.packet.seq_nr == seq_nr)
            .map(|sent| {
                sent.retransmitted = true;
                let mut packet = sent.packet.clone();
                packet.ack_nr = self.ack_nr;
                packet.wnd_size = self.recv_window;
                packet
            });
        if let Some(mut packet) = packet {
            self.stamp_packet(&mut packet);
            if packet.packet_type().ok() == Some(PacketType::StData) {
                self.congestion.on_loss();
            }
            Some(packet)
        } else {
            None
        }
    }

    pub(crate) fn record_packet_sent(&mut self, seq_nr: u16) {
        if let Some(sent) = self
            .send_buffer
            .iter_mut()
            .find(|sent| sent.packet.seq_nr == seq_nr)
        {
            sent.sent_at = Instant::now();
        }
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
        self.update_reply_delay(syn_packet);
        self.seq_nr = rand_connection_id();
        self.syn_seq_nr = self.seq_nr;
        self.state = ConnectionState::Established;

        let mut syn_ack = UtpPacket::syn_ack(
            self.remote_conn_id,
            self.seq_nr,
            self.ack_nr,
            self.recv_window,
        );
        self.stamp_packet(&mut syn_ack);
        self.seq_nr = self.seq_nr.wrapping_add(1);

        Ok(syn_ack)
    }

    /// Repeat the original STATE response when an accepted SYN is retried.
    pub(crate) fn syn_response(&mut self, syn: &UtpPacket) -> Option<UtpPacket> {
        let matches = self.remote_conn_id == syn.connection_id
            && self.accepted_syn_seq == Some(syn.seq_nr)
            && self.local_conn_id == syn.connection_id.wrapping_add(1);
        if !matches {
            return None;
        }
        self.update_reply_delay(syn);
        let mut response = UtpPacket::syn_ack(
            self.remote_conn_id,
            self.syn_seq_nr,
            syn.seq_nr,
            self.recv_window,
        );
        self.stamp_packet(&mut response);
        Some(response)
    }

    /// Close the connection gracefully - creates FIN packet
    pub fn close(&mut self) -> Result<UtpPacket, ConnectionError> {
        if !matches!(
            self.state,
            ConnectionState::Established | ConnectionState::Closing
        ) {
            return Err(ConnectionError::NotConnected);
        }

        let mut fin = UtpPacket::fin(
            self.remote_conn_id,
            self.seq_nr,
            self.ack_nr,
            self.recv_window,
        );
        self.stamp_packet(&mut fin);
        self.send_buffer.push_back(SentPacket {
            packet: fin.clone(),
            sent_at: Instant::now(),
            retransmitted: false,
        });
        self.seq_nr = self.seq_nr.wrapping_add(1);
        self.state = ConnectionState::FinWait;
        self.last_activity = Instant::now();

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

            let mut packet = UtpPacket::data(
                self.remote_conn_id,
                self.seq_nr,
                self.ack_nr,
                self.recv_window,
                data[offset..offset + chunk_size].to_vec(),
            );
            self.stamp_packet(&mut packet);

            self.send_buffer.push_back(SentPacket {
                packet: packet.clone(),
                sent_at: Instant::now(),
                retransmitted: false,
            });
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
        let packet_type = packet.packet_type()?;
        self.update_reply_delay(packet);

        match packet_type {
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
            if !self.syn_retransmitted
                && let Some(sent_at) = self.syn_sent_at.take()
            {
                self.add_rtt_sample(sent_at.elapsed());
            }
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
        let mut fin_acknowledged = false;
        while self
            .send_buffer
            .front()
            .is_some_and(|sent| packet.ack_nr.wrapping_sub(sent.packet.seq_nr) < 0x8000)
        {
            let sent = self.send_buffer.pop_front().unwrap();
            fin_acknowledged |= sent.packet.packet_type().ok() == Some(PacketType::StFin);
            bytes_acknowledged =
                bytes_acknowledged.saturating_add(sent.packet.payload.len() as u32);
            acknowledged.push(sent.packet.seq_nr);
            if !sent.retransmitted {
                self.add_rtt_sample(sent.sent_at.elapsed());
            }
        }
        if bytes_acknowledged > 0 {
            self.congestion.on_ack_received(
                u64::from(packet.timestamp_difference_microseconds),
                bytes_acknowledged,
            );
        }
        if fin_acknowledged {
            self.local_fin_acked = true;
            self.state = ConnectionState::Closed;
        }
        acknowledged
    }

    fn add_rtt_sample(&mut self, sample: Duration) {
        self.rtt_estimator
            .get_or_insert_with(RttEstimator::new)
            .add_sample(sample.as_micros().min(u64::MAX as u128) as u64);
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
        let mut packets = self
            .send_buffer
            .iter()
            .map(|sent| {
                let mut packet = sent.packet.clone();
                packet.ack_nr = self.ack_nr;
                packet.wnd_size = self.recv_window;
                packet
            })
            .collect::<Vec<_>>();
        for packet in &mut packets {
            self.stamp_packet(packet);
        }
        packets
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
        self.rtt_estimator
            .as_ref()
            .map_or(Duration::from_secs(1), |estimator| {
                estimator.rto().max(Duration::from_millis(500))
            })
    }

    /// Get smoothed RTT
    pub fn rtt(&self) -> Duration {
        self.rtt_estimator
            .as_ref()
            .map_or(Duration::from_millis(100), RttEstimator::srtt)
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
mod tests;
