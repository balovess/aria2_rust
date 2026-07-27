//! Bencode serialization for DHT messages (encode path).
//!
//! Implements the BEP 5 wire format encoding using `BencodeValue` from
//! `aria2-protocol`. Produces bencoded dictionaries suitable for UDP transport.
//!
//! C++ reference: `DHTAbstractMessage::getBencodedMessage()`,
//! `DHTQueryMessage::fillMessage()`, `DHTResponseMessage::fillMessage()`.

use std::collections::BTreeMap;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use thiserror::Error;

use super::constants::K;
use super::message::{
    CompactNodeInfo, DhtMessage, MessageTypeKind, key,
};

// ── Error type ────────────────────────────────────────────────────────────

/// Errors that can occur during DHT message encoding/decoding.
#[derive(Debug, Error)]
pub enum MessageCodecError {
    /// Bencode decode failure.
    #[error("bencode decode error: {0}")]
    BencodeDecode(String),

    /// Missing required field in the message dictionary.
    #[error("missing required field: {0}")]
    MissingField(String),

    /// Invalid field value (wrong type, wrong length, etc.).
    #[error("invalid field \"{field}\": {reason}")]
    InvalidField {
        field: String,
        reason: String,
    },

    /// Unknown or unsupported DHT method name.
    #[error("unsupported method: {0}")]
    UnsupportedMethod(String),

    /// Invalid message type indicator ("y" value).
    #[error("invalid message type indicator: {0}")]
    InvalidMessageType(String),
}

pub type Result<T> = std::result::Result<T, MessageCodecError>;

// ── Encoding ──────────────────────────────────────────────────────────────

/// Encode a DHT message to bencoded wire-format bytes.
///
/// C++: `DHTAbstractMessage::getBencodedMessage()`
pub fn encode(msg: &DhtMessage) -> Vec<u8> {
    let mut dict = BTreeMap::new();

    // Common fields: transaction ID and message type
    dict.insert(
        key::T.as_bytes().to_vec(),
        BencodeValue::Bytes(msg.transaction_id().to_vec()),
    );

    match msg.kind() {
        MessageTypeKind::Query => {
            dict.insert(key::Y.as_bytes().to_vec(), BencodeValue::Bytes(b"q".to_vec()));
            dict.insert(
                key::Q.as_bytes().to_vec(),
                BencodeValue::Bytes(msg.method_name().unwrap().as_bytes().to_vec()),
            );
            dict.insert(key::A.as_bytes().to_vec(), encode_query_args(msg));
        }
        MessageTypeKind::Response => {
            dict.insert(key::Y.as_bytes().to_vec(), BencodeValue::Bytes(b"r".to_vec()));
            dict.insert(key::R.as_bytes().to_vec(), encode_response_values(msg));
        }
        MessageTypeKind::Error => {
            dict.insert(key::Y.as_bytes().to_vec(), BencodeValue::Bytes(b"e".to_vec()));
            if let DhtMessage::Error { code, message, .. } = msg {
                dict.insert(
                    key::E.as_bytes().to_vec(),
                    BencodeValue::List(vec![
                        BencodeValue::Int(*code),
                        BencodeValue::Bytes(message.as_bytes().to_vec()),
                    ]),
                );
            }
        }
    }

    // Version field: "A2" + 2-byte big-endian version number
    let version = super::message::make_version_string(0);
    dict.insert(key::V.as_bytes().to_vec(), BencodeValue::Bytes(version));

    BencodeValue::Dict(dict).encode()
}

/// Encode query arguments into a bencoded dictionary.
///
/// C++: `DHTQueryMessage::fillMessage()` puts "q" and "a" keys.
fn encode_query_args(msg: &DhtMessage) -> BencodeValue {
    let mut args = BTreeMap::new();
    let sender_id = msg.sender_id().unwrap();

    // All queries include the sender's node ID
    args.insert(
        key::ID.as_bytes().to_vec(),
        BencodeValue::Bytes(sender_id.as_bytes().to_vec()),
    );

    match msg {
        DhtMessage::PingQuery { .. } => {}
        DhtMessage::FindNodeQuery { payload, .. } => {
            args.insert(
                key::TARGET.as_bytes().to_vec(),
                BencodeValue::Bytes(payload.target.as_bytes().to_vec()),
            );
        }
        DhtMessage::GetPeersQuery { payload, .. } => {
            args.insert(
                key::INFO_HASH.as_bytes().to_vec(),
                BencodeValue::Bytes(payload.info_hash.as_bytes().to_vec()),
            );
        }
        DhtMessage::AnnouncePeerQuery { payload, .. } => {
            args.insert(
                key::INFO_HASH.as_bytes().to_vec(),
                BencodeValue::Bytes(payload.info_hash.as_bytes().to_vec()),
            );
            args.insert(key::PORT.as_bytes().to_vec(), BencodeValue::Int(payload.port as i64));
            args.insert(
                key::TOKEN.as_bytes().to_vec(),
                BencodeValue::Bytes(payload.token.clone()),
            );
        }
        _ => unreachable!("encode_query_args called on non-query message"),
    }

    BencodeValue::Dict(args)
}

/// Encode response return values into a bencoded dictionary.
///
/// C++: `DHTResponseMessage::fillMessage()` puts "r" key.
fn encode_response_values(msg: &DhtMessage) -> BencodeValue {
    let mut resp = BTreeMap::new();
    let sender_id = msg.sender_id().unwrap();

    // All responses include the sender's node ID
    resp.insert(
        key::ID.as_bytes().to_vec(),
        BencodeValue::Bytes(sender_id.as_bytes().to_vec()),
    );

    match msg {
        DhtMessage::PingResponse { .. } => {}
        DhtMessage::FindNodeResponse { payload, .. } => {
            encode_compact_nodes(&mut resp, &payload.nodes);
        }
        DhtMessage::GetPeersResponse { payload, .. } => {
            resp.insert(
                key::TOKEN.as_bytes().to_vec(),
                BencodeValue::Bytes(payload.token.clone()),
            );
            encode_compact_nodes(&mut resp, &payload.nodes);
            if !payload.values.is_empty() {
                let values: Vec<BencodeValue> =
                    payload.values.iter().map(|p| BencodeValue::Bytes(p.pack())).collect();
                resp.insert(key::VALUES.as_bytes().to_vec(), BencodeValue::List(values));
            }
        }
        DhtMessage::AnnouncePeerResponse { .. } => {}
        _ => unreachable!("encode_response_values called on non-response message"),
    }

    BencodeValue::Dict(resp)
}

/// Encode compact node info, separating IPv4 ("nodes") and IPv6 ("nodes6").
///
/// C++: `DHTFindNodeReplyMessage::getResponse()` / `DHTGetPeersReplyMessage::getResponse()`
fn encode_compact_nodes(dict: &mut BTreeMap<Vec<u8>, BencodeValue>, nodes: &[CompactNodeInfo]) {
    if nodes.is_empty() {
        return;
    }

    let mut v4_buf = Vec::new();
    let mut v6_buf = Vec::new();
    let mut v4_count = 0usize;
    let mut v6_count = 0usize;

    for node in nodes {
        if let Some(packed) = node.pack() {
            match node.addr {
                std::net::SocketAddr::V4(_) if v4_count < K => {
                    v4_buf.extend(packed);
                    v4_count += 1;
                }
                std::net::SocketAddr::V6(_) if v6_count < K => {
                    v6_buf.extend(packed);
                    v6_count += 1;
                }
                _ => {}
            }
        }
    }

    if !v4_buf.is_empty() {
        dict.insert(key::NODES.as_bytes().to_vec(), BencodeValue::Bytes(v4_buf));
    }
    if !v6_buf.is_empty() {
        dict.insert(key::NODES6.as_bytes().to_vec(), BencodeValue::Bytes(v6_buf));
    }
}

// ── Bencode value extraction helpers (shared with decode) ─────────────────

/// Extract a byte string value from a dictionary.
pub(crate) fn extract_bytes<'a>(
    dict: &'a BTreeMap<Vec<u8>, BencodeValue>,
    key: &str,
) -> Result<&'a [u8]> {
    dict.get(key.as_bytes())
        .and_then(|v| v.as_bytes())
        .ok_or_else(|| MessageCodecError::MissingField(key.into()))
}

/// Extract a string value from a dictionary.
pub(crate) fn extract_str<'a>(
    dict: &'a BTreeMap<Vec<u8>, BencodeValue>,
    key: &str,
) -> Result<&'a str> {
    dict.get(key.as_bytes())
        .and_then(|v| v.as_str())
        .ok_or_else(|| MessageCodecError::MissingField(key.into()))
}

/// Extract a byte string value from a nested dictionary.
pub(crate) fn extract_bytes_from<'a>(
    dict: &'a BTreeMap<Vec<u8>, BencodeValue>,
    key: &str,
) -> Result<&'a [u8]> {
    dict.get(key.as_bytes())
        .and_then(|v| v.as_bytes())
        .ok_or_else(|| MessageCodecError::MissingField(key.into()))
}

/// Extract an integer value from a nested dictionary.
pub(crate) fn extract_int_from(
    dict: &BTreeMap<Vec<u8>, BencodeValue>,
    key: &str,
) -> Result<i64> {
    dict.get(key.as_bytes())
        .and_then(|v| v.as_int())
        .ok_or_else(|| MessageCodecError::MissingField(key.into()))
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::super::constants::ID_LENGTH;
    use super::super::message::PingQueryPayload;
    use super::super::node_id::NodeId;

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 6881)
    }

    #[test]
    fn encode_ping_query() {
        let msg = DhtMessage::PingQuery {
            transaction_id: vec![0x01, 0x02],
            sender_id: NodeId::from_slice(&[0xAB; ID_LENGTH]),
            sender_addr: test_addr(),
            payload: PingQueryPayload,
        };
        let encoded = encode(&msg);
        // Must be valid bencode starting with 'd' and ending with 'e'
        assert_eq!(encoded[0], b'd');
        assert_eq!(encoded[encoded.len() - 1], b'e');
        assert!(BencodeValue::decode(&encoded).is_ok());
    }

    #[test]
    fn encode_error_message() {
        let msg = DhtMessage::Error {
            transaction_id: vec![0x01],
            sender_addr: test_addr(),
            code: 201,
            message: "some error".into(),
        };
        let encoded = encode(&msg);
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        // Verify "y" = "e"
        let y = decoded.dict_get(b"y").and_then(|v| v.as_str()).unwrap();
        assert_eq!(y, "e");
    }

    #[test]
    fn version_field_present() {
        let msg = DhtMessage::PingQuery {
            transaction_id: vec![0x01],
            sender_id: NodeId::from_slice(&[0xAB; ID_LENGTH]),
            sender_addr: test_addr(),
            payload: PingQueryPayload,
        };
        let encoded = encode(&msg);
        let (decoded, _) = BencodeValue::decode(&encoded).unwrap();
        assert!(decoded.dict_get(b"v").is_some());
    }
}
