//! uTP packet structure and serialization
//!
//! Implements the binary packet format as specified in BEP 29.

use thiserror::Error;

/// uTP protocol version (currently 1)
pub const UTP_VERSION: u8 = 1;

/// Size of uTP header in bytes
pub const UTP_HEADER_SIZE: usize = 20;

const SELECTIVE_ACK_EXTENSION: u8 = 1;

/// Packet type constants as defined in BEP 29
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum PacketType {
    /// DATA packet - contains payload
    StData = 0,
    /// FIN packet - gracefully closes connection
    StFin = 1,
    /// ACK packet - acknowledges received data
    StAck = 2,
    /// RESET packet - abruptly closes connection
    StReset = 3,
    /// SYN packet - initiates connection
    StSyn = 4,
}

impl PacketType {
    /// Convert from u8 to PacketType
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(PacketType::StData),
            1 => Some(PacketType::StFin),
            2 => Some(PacketType::StAck),
            3 => Some(PacketType::StReset),
            4 => Some(PacketType::StSyn),
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
    /// Raw bytes after the fixed header, including extensions and application data.
    pub payload: Vec<u8>,
}

pub(crate) struct PacketExtensions<'a> {
    pub selective_ack: Option<&'a [u8]>,
    pub application_payload: &'a [u8],
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
    pub fn syn(connection_id: u16, seq_nr: u16, ack_nr: u16, wnd_size: u32) -> Self {
        let mut packet = Self::new(PacketType::StSyn);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet.ack_nr = ack_nr;
        packet.wnd_size = wnd_size;
        packet
    }

    /// Create a SYN-ACK packet (server response to SYN)
    pub fn syn_ack(connection_id: u16, seq_nr: u16, ack_nr: u16, wnd_size: u32) -> Self {
        let mut packet = Self::new(PacketType::StAck);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet.ack_nr = ack_nr;
        packet.wnd_size = wnd_size;
        packet
    }

    /// Create a DATA packet
    pub fn data(
        connection_id: u16,
        seq_nr: u16,
        ack_nr: u16,
        wnd_size: u32,
        payload: Vec<u8>,
    ) -> Self {
        let mut packet = Self::new(PacketType::StData);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet.ack_nr = ack_nr;
        packet.wnd_size = wnd_size;
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
    pub fn fin(connection_id: u16, seq_nr: u16, ack_nr: u16, wnd_size: u32) -> Self {
        let mut packet = Self::new(PacketType::StFin);
        packet.connection_id = connection_id;
        packet.seq_nr = seq_nr;
        packet.ack_nr = ack_nr;
        packet.wnd_size = wnd_size;
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

    /// Calculate the local delay sample for this packet's remote send timestamp.
    pub fn calculate_timestamp_diff(&self, local_timestamp: u32) -> u32 {
        local_timestamp.wrapping_sub(self.timestamp_microseconds)
    }

    pub(crate) fn parse_extensions(&self) -> Result<PacketExtensions<'_>, UtpPacketError> {
        let mut extension_type = self.extension;
        let mut offset = 0;
        let mut selective_ack = None;

        while extension_type != 0 {
            if self.payload.len().saturating_sub(offset) < 2 {
                return Err(UtpPacketError::InvalidExtension(extension_type));
            }
            let next_extension = self.payload[offset];
            let extension_length = usize::from(self.payload[offset + 1]);
            let extension_start = offset + 2;
            let extension_end = extension_start
                .checked_add(extension_length)
                .filter(|end| *end <= self.payload.len())
                .ok_or(UtpPacketError::InvalidExtension(extension_type))?;

            if extension_type == SELECTIVE_ACK_EXTENSION {
                if extension_length < 4 || !extension_length.is_multiple_of(4) {
                    return Err(UtpPacketError::InvalidExtension(extension_type));
                }
                if selective_ack.is_some() {
                    return Err(UtpPacketError::InvalidExtension(extension_type));
                }
                selective_ack = Some(&self.payload[extension_start..extension_end]);
            }

            extension_type = next_extension;
            offset = extension_end;
        }

        Ok(PacketExtensions {
            selective_ack,
            application_payload: &self.payload[offset..],
        })
    }

    pub(crate) fn set_selective_ack(&mut self, mask: &[u8]) {
        debug_assert!(mask.len() >= 4);
        debug_assert!(mask.len().is_multiple_of(4));
        debug_assert!(mask.len() <= usize::from(u8::MAX));

        self.extension = SELECTIVE_ACK_EXTENSION;
        self.payload.clear();
        self.payload.reserve(mask.len() + 2);
        self.payload.extend_from_slice(&[0, mask.len() as u8]);
        self.payload.extend_from_slice(mask);
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
mod tests;
