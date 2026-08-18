use std::hash::{Hash, Hasher};

pub const PROTOCOL_STRING: &[u8] = b"BitTorrent protocol";
pub const HANDSHAKE_LENGTH: usize = 68;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Choke = 0,
    Unchoke = 1,
    Interested = 2,
    NotInterested = 3,
    Have = 4,
    Bitfield = 5,
    Request = 6,
    Piece = 7,
    Cancel = 8,
    Port = 9,
    // BEP 6 Fast Extension IDs — must match the standard wire protocol:
    //   Suggest=13, HaveAll=14, HaveNone=15, Reject=16, AllowedFast=17
    Suggest = 13,
    HaveAll = 14,
    HaveNone = 15,
    Reject = 16,
    AllowedFast = 17,
    /// BEP 10: Extension Protocol. ID = 20.
    /// Used for ut_metadata, ut_pex, and other extensions.
    Extended = 20,
}

impl TryFrom<u8> for MessageType {
    type Error = String;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(MessageType::Choke),
            1 => Ok(MessageType::Unchoke),
            2 => Ok(MessageType::Interested),
            3 => Ok(MessageType::NotInterested),
            4 => Ok(MessageType::Have),
            5 => Ok(MessageType::Bitfield),
            6 => Ok(MessageType::Request),
            7 => Ok(MessageType::Piece),
            8 => Ok(MessageType::Cancel),
            9 => Ok(MessageType::Port),
            13 => Ok(MessageType::Suggest),
            14 => Ok(MessageType::HaveAll),
            15 => Ok(MessageType::HaveNone),
            16 => Ok(MessageType::Reject),
            17 => Ok(MessageType::AllowedFast),
            20 => Ok(MessageType::Extended),
            n => Err(format!("Invalid message ID: {}", n)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceBlockRequest {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

impl Hash for PieceBlockRequest {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state);
        self.begin.hash(state);
        self.length.hash(state);
    }
}

impl PieceBlockRequest {
    pub fn new(index: u32, begin: u32, length: u32) -> Self {
        Self {
            index,
            begin,
            length,
        }
    }

    pub fn serialized_size() -> usize {
        12
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum BtMessage {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have {
        piece_index: u32,
    },
    Bitfield {
        data: Vec<u8>,
    },
    Request {
        request: PieceBlockRequest,
    },
    Piece {
        index: u32,
        begin: u32,
        /// Payload owned by the frame buffer. Parsed network messages use a
        /// zero-copy slice of that buffer.
        data: Bytes,
    },
    Cancel {
        request: PieceBlockRequest,
    },
    Port {
        port: u16,
    },
    AllowedFast {
        index: u32,
    },
    Reject {
        index: u32,
        offset: u32,
        length: u32,
    },
    Suggest {
        index: u32,
    },
    HaveAll,
    HaveNone,
    /// BEP 10: Extension Protocol message (ID=20).
    ///
    /// The first byte of the payload is the extended message ID (0 = handshake,
    /// 1+ = application-specific, negotiated via the extension handshake).
    /// The remaining bytes are the bencoded payload.
    ///
    /// This enables ut_metadata (BEP 9), ut_pex (BEP 11), and other extensions.
    Extended {
        /// Extended message ID: 0 for handshake, 1+ for negotiated extensions.
        ext_id: u8,
        /// Bencoded payload data.
        payload: Vec<u8>,
    },
}

impl BtMessage {
    pub fn message_id(&self) -> Option<u8> {
        match self {
            BtMessage::KeepAlive => None,
            BtMessage::Choke => Some(0),
            BtMessage::Unchoke => Some(1),
            BtMessage::Interested => Some(2),
            BtMessage::NotInterested => Some(3),
            BtMessage::Have { .. } => Some(4),
            BtMessage::Bitfield { .. } => Some(5),
            BtMessage::Request { .. } => Some(6),
            BtMessage::Piece { .. } => Some(7),
            BtMessage::Cancel { .. } => Some(8),
            BtMessage::Port { .. } => Some(9),
            BtMessage::Suggest { .. } => Some(13),
            BtMessage::HaveAll => Some(14),
            BtMessage::HaveNone => Some(15),
            BtMessage::Reject { .. } => Some(16),
            BtMessage::AllowedFast { .. } => Some(17),
            BtMessage::Extended { .. } => Some(20),
        }
    }

    pub fn payload_size(&self) -> Option<usize> {
        match self {
            BtMessage::KeepAlive => None,
            BtMessage::Choke
            | BtMessage::Unchoke
            | BtMessage::Interested
            | BtMessage::NotInterested => Some(1),
            BtMessage::Have { .. } => Some(5),
            BtMessage::Bitfield { data } => Some(1 + data.len()),
            BtMessage::Request { .. } | BtMessage::Cancel { .. } => Some(13),
            BtMessage::Piece { data, .. } => Some(9 + data.len()),
            BtMessage::Port { .. } => Some(3),
            BtMessage::AllowedFast { .. } => Some(5),
            BtMessage::Reject { .. } => Some(13),
            BtMessage::Suggest { .. } => Some(5),
            BtMessage::HaveAll | BtMessage::HaveNone => Some(1),
            BtMessage::Extended { payload, .. } => Some(2 + payload.len()),
        }
    }
}
use bytes::Bytes;
