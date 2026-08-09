//! BEP 9 `ut_metadata` extension message types for BitTorrent.
//!
//! Implements the wire-format encoding/decoding for `ut_metadata` messages,
//! which exchange torrent metadata pieces without a `.torrent` file (used by
//! magnet links).
//!
//! This type operates on the *payload* portion of a `BtMessage::Extended`,
//! i.e. the bytes **after** the 1-byte `ext_id` field.

use std::collections::BTreeMap;

use crate::bittorrent::bencode::codec::BencodeValue;

/// BEP 9 ut_metadata extension message.
///
/// The wire format for a ut_metadata payload is a bencoded dict followed
/// (for `Data` messages only) by raw metadata bytes. Specifically:
///
/// - **Request** (msg_type=0): `d8:msg_typei0e5:piecei< N >ee`
/// - **Data**    (msg_type=1): `d8:msg_typei1e5:piecei< N >10:total_sizei< S >ee<raw bytes>`
/// - **Reject**  (msg_type=2): `d8:msg_typei2e5:piecei< N >ee`
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UtMetadataMessage {
    /// Request metadata piece `piece` (msg_type = 0).
    Request { piece: u32 },
    /// Deliver metadata piece `piece` of `total_size` bytes (msg_type = 1).
    /// `data` holds the raw metadata bytes appended *after* the bencoded dict.
    Data {
        piece: u32,
        total_size: u32,
        data: Vec<u8>,
    },
    /// Reject metadata piece request (msg_type = 2).
    Reject { piece: u32 },
}

impl UtMetadataMessage {
    /// Encode this message to the payload bytes (after the ext_id byte).
    ///
    /// For `Data` messages, the raw metadata bytes are appended after the
    /// bencoded dictionary, per BEP 9.
    pub fn to_payload(&self) -> Vec<u8> {
        let mut dict = BTreeMap::new();

        match self {
            UtMetadataMessage::Request { piece } => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(0));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
            }
            UtMetadataMessage::Data {
                piece,
                total_size,
                data: _,
            } => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(1));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
                dict.insert(
                    b"total_size".to_vec(),
                    BencodeValue::Int(*total_size as i64),
                );
            }
            UtMetadataMessage::Reject { piece } => {
                dict.insert(b"msg_type".to_vec(), BencodeValue::Int(2));
                dict.insert(b"piece".to_vec(), BencodeValue::Int(*piece as i64));
            }
        }

        let mut payload = BencodeValue::Dict(dict).encode();

        // For Data messages, append the raw metadata bytes after the bencoded dict.
        if let UtMetadataMessage::Data { data, .. } = self {
            payload.extend_from_slice(data);
        }

        payload
    }

    /// Parse a ut_metadata payload (the bytes after the ext_id byte).
    ///
    /// For `Data` messages, the raw metadata piece is the trailing bytes
    /// after the bencoded dict.
    pub fn from_payload(payload: &[u8]) -> Result<Self, String> {
        let (val, consumed) = BencodeValue::decode(payload)
            .map_err(|e| format!("Failed to decode ut_metadata payload: {}", e))?;

        let dict = val
            .as_dict()
            .ok_or("ut_metadata payload is not a bencoded dict")?;

        let msg_type = dict
            .get(b"msg_type".as_slice())
            .and_then(|v| v.as_int())
            .ok_or("Missing 'msg_type' in ut_metadata payload")? as u32;

        let piece = dict
            .get(b"piece".as_slice())
            .and_then(|v| v.as_int())
            .ok_or("Missing 'piece' in ut_metadata payload")? as u32;

        match msg_type {
            0 => Ok(UtMetadataMessage::Request { piece }),
            1 => {
                let total_size = dict
                    .get(b"total_size".as_slice())
                    .and_then(|v| v.as_int())
                    .ok_or("Missing 'total_size' in ut_metadata Data message")?
                    as u32;

                // The raw metadata bytes follow the bencoded dict.
                let data = payload[consumed..].to_vec();

                Ok(UtMetadataMessage::Data {
                    piece,
                    total_size,
                    data,
                })
            }
            2 => Ok(UtMetadataMessage::Reject { piece }),
            _ => Err(format!("Unknown ut_metadata msg_type: {}", msg_type)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ======================== UtMetadataMessage tests ========================

    #[test]
    fn test_ut_metadata_request_roundtrip() {
        let msg = UtMetadataMessage::Request { piece: 0 };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_request_nonzero_piece() {
        let msg = UtMetadataMessage::Request { piece: 42 };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, UtMetadataMessage::Request { piece: 42 });
    }

    #[test]
    fn test_ut_metadata_data_roundtrip() {
        let metadata_piece = b"fake torrent metadata bytes".to_vec();
        let msg = UtMetadataMessage::Data {
            piece: 3,
            total_size: 50000,
            data: metadata_piece,
        };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_data_empty_piece() {
        let msg = UtMetadataMessage::Data {
            piece: 0,
            total_size: 0,
            data: Vec::new(),
        };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_reject_roundtrip() {
        let msg = UtMetadataMessage::Reject { piece: 7 };
        let payload = msg.to_payload();
        let parsed = UtMetadataMessage::from_payload(&payload).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn test_ut_metadata_missing_msg_type() {
        // Dict without msg_type
        let mut dict = BTreeMap::new();
        dict.insert(b"piece".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("msg_type"));
    }

    #[test]
    fn test_ut_metadata_missing_piece() {
        // Dict without piece
        let mut dict = BTreeMap::new();
        dict.insert(b"msg_type".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("piece"));
    }

    #[test]
    fn test_ut_metadata_data_missing_total_size() {
        // Data message without total_size
        let mut dict = BTreeMap::new();
        dict.insert(b"msg_type".to_vec(), BencodeValue::Int(1));
        dict.insert(b"piece".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("total_size"));
    }

    #[test]
    fn test_ut_metadata_unknown_msg_type() {
        let mut dict = BTreeMap::new();
        dict.insert(b"msg_type".to_vec(), BencodeValue::Int(99));
        dict.insert(b"piece".to_vec(), BencodeValue::Int(0));
        let payload = BencodeValue::Dict(dict).encode();
        let result = UtMetadataMessage::from_payload(&payload);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Unknown"));
    }

    #[test]
    fn test_ut_metadata_invalid_bencode() {
        let result = UtMetadataMessage::from_payload(b"garbage");
        assert!(result.is_err());
    }
}
