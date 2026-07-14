//! uTP packet structure and serialization
//!
//! Implements the binary packet format as specified in BEP 29.

use thiserror::Error;

/// uTP protocol version (currently 1)
pub const UTP_VERSION: u8 = 1;

/// Size of uTP header in bytes
pub const UTP_HEADER_SIZE: usize = 20;

/// Packet type constants as defined in BEP 29
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketType {
    /// SYN packet - initiates connection
    StSyn = 0,
    /// DATA packet - contains payload
    StData = 1,
    /// ACK packet - acknowledges received data
    StAck = 2,
    /// FIN packet - gracefully closes connection
    StFin = 3,
    /// RESET packet - abruptly closes connection
    StReset = 4,
}

impl PacketType {
    /// Convert from u8 to PacketType
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(PacketType::StSyn),
            1 => Some(PacketType::StData),
            2 => Some(PacketType::StAck),
            3 => Some(PacketType::StFin),
            4 => Some(PacketType::StReset),
            _ => None,
        }
    }

    /// Convert to u8
    pub fn to_u8(self) -> u8 {
        self as u8
    }

    /// Check if this packet type can carry payload
    pub fn has_payload(&self) -> bool {
        matches!(self, PacketType::StData | PacketType::StSyn)
    }
}

impl std::fmt::Display for PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PacketType::StSyn => write!(f, "SYN"),
            PacketType::StData => write!(f, "DATA"),
            PacketType::StAck => write!(f, "ACK"),
            PacketType::StFin => write!(f, "FIN"),
            PacketType::StReset => write!(f, "RESET"),
        }
    }
}

/// Errors that can occur during packet operations
#[derive(Debug, Error)]
pub enum UtpPacketError {
    #[error("Buffer too small: expected at least {expected} bytes, got {actual}")]
    BufferTooSmall { expected: usize, actual: usize },

    #[error("Invalid packet type: {0}")]
    InvalidPacketType(u8),

    #[error("Invalid version: expected {expected}, got {actual}")]
    InvalidVersion { expected: u8, actual: u8 },

    #[error("Invalid extension: {0}")]
    InvalidExtension(u8),
}

/// uTP packet header structure
///
/// The uTP header format (20 bytes):
/// ```text
/// 0                   1                   2                   3
/// 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// | type | ver(4) | extension     | connection_id                 |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// | timestamp_microseconds                                        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// | timestamp_difference_microseconds                             |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// | wnd_size                                                      |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// | seq_nr                        | ack_nr                        |
/// +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtpPacket {
    /// Packet type (4 bits) and version (4 bits) combined in first byte
    pub type_ver: u8,
    /// Extension byte (0 = no extension)
    pub extension: u8,
    /// Connection ID (16 bits, big-endian)
    pub connection_id: u16,
    /// Timestamp in microseconds (32 bits, big-endian)
    pub timestamp_microseconds: u32,
    /// Timestamp difference in microseconds (32 bits, big-endian)
    pub timestamp_difference_microseconds: u32,
    /// Advertised window size in bytes (32 bits, big-endian)
    pub wnd_size: u32,
    /// Sequence number (16 bits, big-endian)
    pub seq_nr: u16,
    /// Acknowledgment number (16 bits, big-endian)
    pub ack_nr: u16,
    /// Optional payload (only for DATA and SYN packets)
    pub payload: Vec<u8>,
}

impl UtpPacket {
    /// Create a new uTP packet with the given type
    pub fn new(packet_type: PacketType) -> Self {
        Self {
            type_ver: (packet_type.to_u8() << 4) | UTP_VERSION,
            extension: 0,
            connection_id: 0,
            timestamp_microseconds: 0,
            timestamp_difference_microseconds: 0,
            wnd_size: 0,
            seq_nr: 0,
            ack_nr: 0,
            payload: Vec::new(),
        }
    }

    /// Create a SYN packet
    pub fn syn(connection_id: u16, seq_nr: u16) -> Self {
        let mut packet = Self::new(PacketType::StSyn);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet
    }

    /// Create a DATA packet
    pub fn data(connection_id: u16, seq_nr: u16, ack_nr: u16, payload: Vec<u8>) -> Self {
        let mut packet = Self::new(PacketType::StData);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet.ack_nr = ack_nr;
        packet.payload = payload;
        packet
    }

    /// Create an ACK packet
    pub fn ack(connection_id: u16, ack_nr: u16, seq_nr: u16, wnd_size: u32) -> Self {
        let mut packet = Self::new(PacketType::StAck);
        packet.connection_id = connection_id;
        packet.ack_nr = ack_nr;
        packet.seq_nr = seq_nr;
        packet.wnd_size = wnd_size;
        packet
    }

    /// Create a FIN packet
    pub fn fin(connection_id: u16, seq_nr: u16, ack_nr: u16) -> Self {
        let mut packet = Self::new(PacketType::StFin);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet.ack_nr = ack_nr;
        packet
    }

    /// Create a RESET packet
    pub fn reset(connection_id: u16) -> Self {
        let mut packet = Self::new(PacketType::StReset);
        packet.connection_id = connection_id;
        packet
    }

    /// Get the packet type
    pub fn packet_type(&self) -> Result<PacketType, UtpPacketError> {
        let type_bits = self.type_ver >> 4;
        PacketType::from_u8(type_bits).ok_or(UtpPacketError::InvalidPacketType(type_bits))
    }

    /// Set the packet type
    pub fn set_packet_type(&mut self, packet_type: PacketType) {
        self.type_ver = (packet_type.to_u8() << 4) | (self.type_ver & 0x0F);
    }

    /// Get the protocol version
    pub fn version(&self) -> u8 {
        self.type_ver & 0x0F
    }

    /// Set the protocol version
    pub fn set_version(&mut self, version: u8) {
        self.type_ver = (self.type_ver & 0xF0) | (version & 0x0F);
    }

    /// Get the total packet size (header + payload)
    pub fn total_size(&self) -> usize {
        UTP_HEADER_SIZE + self.payload.len()
    }

    /// Check if the packet has a valid version
    pub fn is_valid_version(&self) -> bool {
        self.version() == UTP_VERSION
    }

    /// Serialize the packet to bytes (big-endian format)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.total_size());

        // Byte 0: type (4 bits) | version (4 bits)
        buf.push(self.type_ver);

        // Byte 1: extension
        buf.push(self.extension);

        // Bytes 2-3: connection_id (big-endian)
        buf.extend_from_slice(&self.connection_id.to_be_bytes());

        // Bytes 4-7: timestamp_microseconds (big-endian)
        buf.extend_from_slice(&self.timestamp_microseconds.to_be_bytes());

        // Bytes 8-11: timestamp_difference_microseconds (big-endian)
        buf.extend_from_slice(&self.timestamp_difference_microseconds.to_be_bytes());

        // Bytes 12-15: wnd_size (big-endian)
        buf.extend_from_slice(&self.wnd_size.to_be_bytes());

        // Bytes 16-17: seq_nr (big-endian)
        buf.extend_from_slice(&self.seq_nr.to_be_bytes());

        // Bytes 18-19: ack_nr (big-endian)
        buf.extend_from_slice(&self.ack_nr.to_be_bytes());

        // Payload (if any)
        buf.extend_from_slice(&self.payload);

        buf
    }

    /// Deserialize a packet from bytes (big-endian format)
    pub fn from_bytes(data: &[u8]) -> Result<Self, UtpPacketError> {
        if data.len() < UTP_HEADER_SIZE {
            return Err(UtpPacketError::BufferTooSmall {
                expected: UTP_HEADER_SIZE,
                actual: data.len(),
            });
        }

        let type_ver = data[0];
        let version = type_ver & 0x0F;

        if version != UTP_VERSION {
            return Err(UtpPacketError::InvalidVersion {
                expected: UTP_VERSION,
                actual: version,
            });
        }

        let extension = data[1];

        // Parse big-endian fields
        let connection_id = u16::from_be_bytes([data[2], data[3]]);
        let timestamp_microseconds = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let timestamp_difference_microseconds =
            u32::from_be_bytes([data[8], data[9], data[10], data[11]]);
        let wnd_size = u32::from_be_bytes([data[12], data[13], data[14], data[15]]);
        let seq_nr = u16::from_be_bytes([data[16], data[17]]);
        let ack_nr = u16::from_be_bytes([data[18], data[19]]);

        // Extract payload (everything after header)
        let payload = data[UTP_HEADER_SIZE..].to_vec();

        Ok(Self {
            type_ver,
            extension,
            connection_id,
            timestamp_microseconds,
            timestamp_difference_microseconds,
            wnd_size,
            seq_nr,
            ack_nr,
            payload,
        })
    }

    /// Calculate the timestamp difference from a remote timestamp
    pub fn calculate_timestamp_diff(&self, remote_timestamp: u32) -> u32 {
        // Timestamp difference is the time since the remote packet was sent
        // This is used for delay calculation
        remote_timestamp.wrapping_sub(self.timestamp_difference_microseconds)
    }
}

impl std::fmt::Display for UtpPacket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let packet_type = self
            .packet_type()
            .map(|t| t.to_string())
            .unwrap_or_else(|_| "INVALID".to_string());
        write!(
            f,
            "UtpPacket {{ type: {}, conn_id: {}, seq: {}, ack: {}, wnd: {}, ts: {}, ts_diff: {}, payload: {} bytes }}",
            packet_type,
            self.connection_id,
            self.seq_nr,
            self.ack_nr,
            self.wnd_size,
            self.timestamp_microseconds,
            self.timestamp_difference_microseconds,
            self.payload.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_packet_type_conversion() {
        assert_eq!(PacketType::from_u8(0), Some(PacketType::StSyn));
        assert_eq!(PacketType::from_u8(1), Some(PacketType::StData));
        assert_eq!(PacketType::from_u8(2), Some(PacketType::StAck));
        assert_eq!(PacketType::from_u8(3), Some(PacketType::StFin));
        assert_eq!(PacketType::from_u8(4), Some(PacketType::StReset));
        assert_eq!(PacketType::from_u8(5), None);

        assert_eq!(PacketType::StSyn.to_u8(), 0);
        assert_eq!(PacketType::StData.to_u8(), 1);
        assert_eq!(PacketType::StAck.to_u8(), 2);
        assert_eq!(PacketType::StFin.to_u8(), 3);
        assert_eq!(PacketType::StReset.to_u8(), 4);
    }

    #[test]
    fn test_packet_type_has_payload() {
        assert!(PacketType::StSyn.has_payload());
        assert!(PacketType::StData.has_payload());
        assert!(!PacketType::StAck.has_payload());
        assert!(!PacketType::StFin.has_payload());
        assert!(!PacketType::StReset.has_payload());
    }

    #[test]
    fn test_syn_packet_creation() {
        let packet = UtpPacket::syn(12345, 1);
        assert_eq!(packet.packet_type().unwrap(), PacketType::StSyn);
        assert_eq!(packet.connection_id, 12345);
        assert_eq!(packet.seq_nr, 1);
        assert_eq!(packet.version(), UTP_VERSION);
    }

    #[test]
    fn test_data_packet_creation() {
        let payload = vec![1, 2, 3, 4, 5];
        let packet = UtpPacket::data(100, 10, 5, payload.clone());
        assert_eq!(packet.packet_type().unwrap(), PacketType::StData);
        assert_eq!(packet.connection_id, 100);
        assert_eq!(packet.seq_nr, 10);
        assert_eq!(packet.ack_nr, 5);
        assert_eq!(packet.payload, payload);
    }

    #[test]
    fn test_ack_packet_creation() {
        let packet = UtpPacket::ack(200, 15, 20, 1024);
        assert_eq!(packet.packet_type().unwrap(), PacketType::StAck);
        assert_eq!(packet.connection_id, 200);
        assert_eq!(packet.ack_nr, 15);
        assert_eq!(packet.seq_nr, 20);
        assert_eq!(packet.wnd_size, 1024);
    }

    #[test]
    fn test_fin_packet_creation() {
        let packet = UtpPacket::fin(300, 100, 99);
        assert_eq!(packet.packet_type().unwrap(), PacketType::StFin);
        assert_eq!(packet.connection_id, 300);
        assert_eq!(packet.seq_nr, 100);
        assert_eq!(packet.ack_nr, 99);
    }

    #[test]
    fn test_reset_packet_creation() {
        let packet = UtpPacket::reset(400);
        assert_eq!(packet.packet_type().unwrap(), PacketType::StReset);
        assert_eq!(packet.connection_id, 400);
    }

    #[test]
    fn test_packet_serialization_deserialization() {
        let original = UtpPacket {
            type_ver: (PacketType::StData.to_u8() << 4) | UTP_VERSION,
            extension: 0,
            connection_id: 0x1234,
            timestamp_microseconds: 0xDEADBEEF,
            timestamp_difference_microseconds: 0xCAFEBABE,
            wnd_size: 0x10000,
            seq_nr: 0x5678,
            ack_nr: 0x9ABC,
            payload: vec![1, 2, 3, 4, 5],
        };

        let bytes = original.to_bytes();
        let decoded = UtpPacket::from_bytes(&bytes).unwrap();

        assert_eq!(decoded.type_ver, original.type_ver);
        assert_eq!(decoded.extension, original.extension);
        assert_eq!(decoded.connection_id, original.connection_id);
        assert_eq!(
            decoded.timestamp_microseconds,
            original.timestamp_microseconds
        );
        assert_eq!(
            decoded.timestamp_difference_microseconds,
            original.timestamp_difference_microseconds
        );
        assert_eq!(decoded.wnd_size, original.wnd_size);
        assert_eq!(decoded.seq_nr, original.seq_nr);
        assert_eq!(decoded.ack_nr, original.ack_nr);
        assert_eq!(decoded.payload, original.payload);
    }

    #[test]
    fn test_packet_header_size() {
        let packet = UtpPacket::syn(1, 1);
        let bytes = packet.to_bytes();
        assert_eq!(bytes.len(), UTP_HEADER_SIZE);
    }

    #[test]
    fn test_packet_with_payload_size() {
        let payload = vec![0u8; 100];
        let packet = UtpPacket::data(1, 1, 1, payload);
        let bytes = packet.to_bytes();
        assert_eq!(bytes.len(), UTP_HEADER_SIZE + 100);
    }

    #[test]
    fn test_buffer_too_small_error() {
        let data = [0u8; 10];
        let result = UtpPacket::from_bytes(&data);
        assert!(matches!(
            result,
            Err(UtpPacketError::BufferTooSmall {
                expected: UTP_HEADER_SIZE,
                actual: 10
            })
        ));
    }

    #[test]
    fn test_invalid_version_error() {
        // Create packet with invalid version
        let mut data = vec![0u8; UTP_HEADER_SIZE];
        data[0] = (PacketType::StData.to_u8() << 4) | 2; // version 2 is invalid

        let result = UtpPacket::from_bytes(&data);
        assert!(matches!(
            result,
            Err(UtpPacketError::InvalidVersion {
                expected: UTP_VERSION,
                actual: 2
            })
        ));
    }

    #[test]
    fn test_set_packet_type() {
        let mut packet = UtpPacket::new(PacketType::StSyn);
        assert_eq!(packet.packet_type().unwrap(), PacketType::StSyn);

        packet.set_packet_type(PacketType::StData);
        assert_eq!(packet.packet_type().unwrap(), PacketType::StData);
        assert_eq!(packet.version(), UTP_VERSION); // Version should remain unchanged
    }

    #[test]
    fn test_set_version() {
        let mut packet = UtpPacket::new(PacketType::StSyn);
        packet.set_version(UTP_VERSION);
        assert_eq!(packet.version(), UTP_VERSION);
    }

    #[test]
    fn test_is_valid_version() {
        let packet = UtpPacket::new(PacketType::StSyn);
        assert!(packet.is_valid_version());
    }

    #[test]
    fn test_packet_display() {
        let packet = UtpPacket::syn(12345, 1);
        let display = format!("{}", packet);
        assert!(display.contains("SYN"));
        assert!(display.contains("12345"));
    }

    #[test]
    fn test_empty_payload_packet() {
        let packet = UtpPacket::ack(1, 1, 1, 1024);
        assert!(packet.payload.is_empty());
        let decoded = UtpPacket::from_bytes(&packet.to_bytes()).unwrap();
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn test_large_payload_packet() {
        let payload = vec![0xAB; 1400]; // Typical MTU-sized payload
        let packet = UtpPacket::data(1, 1, 1, payload.clone());
        let bytes = packet.to_bytes();
        let decoded = UtpPacket::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.payload, payload);
    }

    #[test]
    fn test_packet_type_display() {
        assert_eq!(format!("{}", PacketType::StSyn), "SYN");
        assert_eq!(format!("{}", PacketType::StData), "DATA");
        assert_eq!(format!("{}", PacketType::StAck), "ACK");
        assert_eq!(format!("{}", PacketType::StFin), "FIN");
        assert_eq!(format!("{}", PacketType::StReset), "RESET");
    }
}
