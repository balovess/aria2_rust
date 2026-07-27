//! Bencode deserialization for DHT messages (decode path).
//!
//! Reads bencoded dictionaries and produces `DhtMessage` instances.
//! The decoder extracts the "y" key to determine the message category
//! (query/response/error), then dispatches to type-specific parsing.
//!
//! C++ reference: `DHTMessageFactoryImpl::createQueryMessage()`,
//! `DHTMessageFactoryImpl::createResponseMessage()`.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use tracing::trace;

use super::constants::ID_LENGTH;
use super::message::{
    AnnouncePeerQueryPayload, AnnouncePeerResponsePayload, CompactNodeInfo, CompactPeerInfo,
    DhtMessage, FindNodeQueryPayload, FindNodeResponsePayload, GetPeersQueryPayload,
    GetPeersResponsePayload, PingQueryPayload, PingResponsePayload, key, method,
};
use super::message_codec::{
    MessageCodecError, Result, extract_bytes, extract_bytes_from, extract_int_from, extract_str,
};
use super::node_id::NodeId;

// ── Public decode API ─────────────────────────────────────────────────────

/// Decode a DHT message from bencoded wire-format bytes.
///
/// Requires the sender's IP address and port (from the UDP datagram
/// source address, not the message body).
///
/// C++: `DHTMessageFactoryImpl::createQueryMessage/createResponseMessage`
pub fn decode(data: &[u8], sender_addr: SocketAddr) -> Result<DhtMessage> {
    let (value, _) = BencodeValue::decode(data).map_err(MessageCodecError::BencodeDecode)?;

    let dict = value
        .as_dict()
        .ok_or_else(|| MessageCodecError::InvalidField {
            field: "root".into(),
            reason: "expected bencoded dictionary".into(),
        })?;

    let transaction_id = extract_bytes(dict, key::T)?;

    let y_str = dict
        .get(key::Y.as_bytes())
        .and_then(|v| v.as_str())
        .ok_or_else(|| MessageCodecError::MissingField(key::Y.into()))?;

    match y_str {
        "q" => decode_query(dict, &transaction_id, sender_addr),
        "r" => decode_response(dict, &transaction_id, sender_addr),
        "e" => decode_error(dict, &transaction_id, sender_addr),
        other => Err(MessageCodecError::InvalidMessageType(other.into())),
    }
}

/// Decode a response message with a known method name.
///
/// Response messages don't include the method name in the wire format.
/// The caller must provide it (typically from a transaction ID tracker
/// that maps txn IDs to their pending query method names).
///
/// C++: `DHTMessageFactoryImpl::createResponseMessage()`
pub fn decode_response_with_method(
    data: &[u8],
    sender_addr: SocketAddr,
    method_name: &str,
) -> Result<DhtMessage> {
    let (value, _) = BencodeValue::decode(data).map_err(MessageCodecError::BencodeDecode)?;

    let dict = value
        .as_dict()
        .ok_or_else(|| MessageCodecError::InvalidField {
            field: "root".into(),
            reason: "expected bencoded dictionary".into(),
        })?;

    let transaction_id = extract_bytes(dict, key::T)?;
    decode_response_inner(dict, &transaction_id, sender_addr, method_name)
}

// ── Query decoding ────────────────────────────────────────────────────────

/// Decode a query message from a bencoded dictionary.
fn decode_query(
    dict: &BTreeMap<Vec<u8>, BencodeValue>,
    transaction_id: &[u8],
    sender_addr: SocketAddr,
) -> Result<DhtMessage> {
    let method_name = extract_str(dict, key::Q)?;

    let args = dict
        .get(key::A.as_bytes())
        .and_then(|v| v.as_dict())
        .ok_or_else(|| MessageCodecError::MissingField(key::A.into()))?;

    let sender_id = extract_node_id(args, key::ID)?;

    match method_name {
        method::PING => Ok(DhtMessage::PingQuery {
            transaction_id: transaction_id.to_vec(),
            sender_id,
            sender_addr,
            payload: PingQueryPayload,
        }),

        method::FIND_NODE => {
            let target = extract_node_id(args, key::TARGET)?;
            Ok(DhtMessage::FindNodeQuery {
                transaction_id: transaction_id.to_vec(),
                sender_id,
                sender_addr,
                payload: FindNodeQueryPayload { target },
            })
        }

        method::GET_PEERS => {
            let info_hash = extract_node_id(args, key::INFO_HASH)?;
            Ok(DhtMessage::GetPeersQuery {
                transaction_id: transaction_id.to_vec(),
                sender_id,
                sender_addr,
                payload: GetPeersQueryPayload { info_hash },
            })
        }

        method::ANNOUNCE_PEER => {
            let info_hash = extract_node_id(args, key::INFO_HASH)?;
            let port = extract_int_from(args, key::PORT)?;
            if !(0 < port && port < u16::MAX as i64) {
                return Err(MessageCodecError::InvalidField {
                    field: key::PORT.into(),
                    reason: format!("port {} out of range (1-65534)", port),
                });
            }
            let token = extract_bytes_from(args, key::TOKEN)?;
            Ok(DhtMessage::AnnouncePeerQuery {
                transaction_id: transaction_id.to_vec(),
                sender_id,
                sender_addr,
                payload: AnnouncePeerQueryPayload {
                    info_hash,
                    port: port as u16,
                    token: token.to_vec(),
                },
            })
        }

        other => Err(MessageCodecError::UnsupportedMethod(other.into())),
    }
}

// ── Response decoding ─────────────────────────────────────────────────────

/// Internal: decode a response with known method name.
fn decode_response_inner(
    dict: &BTreeMap<Vec<u8>, BencodeValue>,
    transaction_id: &[u8],
    sender_addr: SocketAddr,
    method_name: &str,
) -> Result<DhtMessage> {
    let resp = dict
        .get(key::R.as_bytes())
        .and_then(|v| v.as_dict())
        .ok_or_else(|| MessageCodecError::MissingField(key::R.into()))?;

    let sender_id = extract_node_id(resp, key::ID)?;

    match method_name {
        method::PING => Ok(DhtMessage::PingResponse {
            transaction_id: transaction_id.to_vec(),
            sender_id,
            sender_addr,
            payload: PingResponsePayload,
        }),

        method::FIND_NODE => {
            let nodes = decode_compact_nodes(resp);
            Ok(DhtMessage::FindNodeResponse {
                transaction_id: transaction_id.to_vec(),
                sender_id,
                sender_addr,
                payload: FindNodeResponsePayload { nodes },
            })
        }

        method::GET_PEERS => {
            let token = extract_bytes_from(resp, key::TOKEN).unwrap_or_default();
            let nodes = decode_compact_nodes(resp);
            let values = decode_compact_peers(resp);
            Ok(DhtMessage::GetPeersResponse {
                transaction_id: transaction_id.to_vec(),
                sender_id,
                sender_addr,
                payload: GetPeersResponsePayload {
                    token: token.to_vec(),
                    nodes,
                    values,
                },
            })
        }

        method::ANNOUNCE_PEER => Ok(DhtMessage::AnnouncePeerResponse {
            transaction_id: transaction_id.to_vec(),
            sender_id,
            sender_addr,
            payload: AnnouncePeerResponsePayload,
        }),

        other => Err(MessageCodecError::UnsupportedMethod(other.into())),
    }
}

/// Decode a response when the method name is not known.
///
/// Infers the method from the response structure. Production code should
/// use `decode_response_with_method` with the tracked method name.
fn decode_response(
    dict: &BTreeMap<Vec<u8>, BencodeValue>,
    transaction_id: &[u8],
    sender_addr: SocketAddr,
) -> Result<DhtMessage> {
    let inferred_method = infer_response_method(dict);
    trace!(
        txn = %hex::encode(transaction_id),
        method = inferred_method,
        "Decoding DHT response with inferred method"
    );
    decode_response_inner(dict, transaction_id, sender_addr, &inferred_method)
}

/// Infer DHT method from response structure heuristics.
///
/// - "token" present -> get_peers
/// - "nodes"/"nodes6" without "token" -> find_node
/// - Only "id" -> ping (default for minimal responses)
pub(crate) fn infer_response_method(dict: &BTreeMap<Vec<u8>, BencodeValue>) -> String {
    let resp = match dict.get(key::R.as_bytes()).and_then(|v| v.as_dict()) {
        Some(r) => r,
        None => return method::PING.into(),
    };

    if resp.contains_key(key::TOKEN.as_bytes()) {
        return method::GET_PEERS.into();
    }
    if resp.contains_key(key::NODES.as_bytes()) || resp.contains_key(key::NODES6.as_bytes()) {
        return method::FIND_NODE.into();
    }
    if resp.contains_key(key::VALUES.as_bytes()) {
        return method::GET_PEERS.into();
    }
    method::PING.into()
}

// ── Error decoding ────────────────────────────────────────────────────────

/// Decode an error message: `e = [code, message_string]`.
///
/// C++: error handling in `DHTMessageFactoryImpl::createResponseMessage()`
fn decode_error(
    dict: &BTreeMap<Vec<u8>, BencodeValue>,
    transaction_id: &[u8],
    sender_addr: SocketAddr,
) -> Result<DhtMessage> {
    let e_list = dict
        .get(key::E.as_bytes())
        .and_then(|v| v.as_list())
        .ok_or_else(|| MessageCodecError::MissingField(key::E.into()))?;

    if e_list.len() < 2 {
        return Err(MessageCodecError::InvalidField {
            field: key::E.into(),
            reason: "expected list of [code, message]".into(),
        });
    }

    let code = e_list[0]
        .as_int()
        .ok_or_else(|| MessageCodecError::InvalidField {
            field: "e[0]".into(),
            reason: "error code must be integer".into(),
        })?;

    let message = e_list[1]
        .as_str()
        .unwrap_or("<non-utf8 error message>")
        .to_string();

    Ok(DhtMessage::Error {
        transaction_id: transaction_id.to_vec(),
        sender_addr,
        code,
        message,
    })
}

// ── Compact format decoding ───────────────────────────────────────────────

/// Decode compact node info from "nodes" and/or "nodes6" keys.
fn decode_compact_nodes(resp: &BTreeMap<Vec<u8>, BencodeValue>) -> Vec<CompactNodeInfo> {
    let mut nodes = Vec::new();

    if let Some(data) = resp.get(key::NODES.as_bytes()).and_then(|v| v.as_bytes()) {
        nodes.extend(CompactNodeInfo::unpack_all(data));
    }
    if let Some(data) = resp.get(key::NODES6.as_bytes()).and_then(|v| v.as_bytes()) {
        nodes.extend(CompactNodeInfo::unpack_all(data));
    }

    nodes
}

/// Decode compact peer info from "values" key.
///
/// C++: `DHTMessageFactoryImpl::createGetPeersReplyMessage()`
fn decode_compact_peers(resp: &BTreeMap<Vec<u8>, BencodeValue>) -> Vec<CompactPeerInfo> {
    let list = match resp.get(key::VALUES.as_bytes()).and_then(|v| v.as_list()) {
        Some(l) => l,
        None => return Vec::new(),
    };
    list.iter()
        .filter_map(|v| CompactPeerInfo::unpack(v.as_bytes()?))
        .collect()
}

// ── Shared helpers ────────────────────────────────────────────────────────

/// Extract a 20-byte node ID from a nested dictionary, with length validation.
fn extract_node_id(dict: &BTreeMap<Vec<u8>, BencodeValue>, key: &str) -> Result<NodeId> {
    let bytes = extract_bytes_from(dict, key)?;
    if bytes.len() != ID_LENGTH {
        return Err(MessageCodecError::InvalidField {
            field: key.into(),
            reason: format!("expected {} bytes, got {}", ID_LENGTH, bytes.len()),
        });
    }
    Ok(NodeId::from_slice(bytes))
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use super::super::constants::ID_LENGTH;
    use super::super::message::MessageTypeKind;
    use super::super::message_codec::encode;

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 6881)
    }

    fn test_id(byte: u8) -> NodeId {
        NodeId::from_slice(&[byte; ID_LENGTH])
    }

    // ── Roundtrip tests ────────────────────────────────────────────────

    #[test]
    fn roundtrip_ping_query() {
        let msg = DhtMessage::PingQuery {
            transaction_id: vec![0x01, 0x02],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: PingQueryPayload,
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded, test_addr()).unwrap();
        assert_eq!(decoded.kind(), MessageTypeKind::Query);
        assert_eq!(decoded.method_name(), Some(method::PING));
        assert_eq!(decoded.transaction_id(), &[0x01, 0x02]);
        assert_eq!(decoded.sender_id(), Some(&test_id(0xAB)));
    }

    #[test]
    fn roundtrip_find_node_query() {
        let msg = DhtMessage::FindNodeQuery {
            transaction_id: vec![0x03, 0x04],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: FindNodeQueryPayload {
                target: test_id(0xCD),
            },
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded, test_addr()).unwrap();
        assert_eq!(decoded.method_name(), Some(method::FIND_NODE));
        if let DhtMessage::FindNodeQuery { payload, .. } = decoded {
            assert_eq!(payload.target, test_id(0xCD));
        } else {
            panic!("expected FindNodeQuery");
        }
    }

    #[test]
    fn roundtrip_get_peers_query() {
        let msg = DhtMessage::GetPeersQuery {
            transaction_id: vec![0x05, 0x06],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: GetPeersQueryPayload {
                info_hash: test_id(0xEF),
            },
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded, test_addr()).unwrap();
        assert_eq!(decoded.method_name(), Some(method::GET_PEERS));
        if let DhtMessage::GetPeersQuery { payload, .. } = decoded {
            assert_eq!(payload.info_hash, test_id(0xEF));
        } else {
            panic!("expected GetPeersQuery");
        }
    }

    #[test]
    fn roundtrip_announce_peer_query() {
        let msg = DhtMessage::AnnouncePeerQuery {
            transaction_id: vec![0x07, 0x08],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: AnnouncePeerQueryPayload {
                info_hash: test_id(0x11),
                port: 6881,
                token: vec![0x42, 0x43],
            },
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded, test_addr()).unwrap();
        if let DhtMessage::AnnouncePeerQuery { payload, .. } = decoded {
            assert_eq!(payload.port, 6881);
            assert_eq!(payload.token, vec![0x42, 0x43]);
        } else {
            panic!("expected AnnouncePeerQuery");
        }
    }

    #[test]
    fn roundtrip_ping_response() {
        let msg = DhtMessage::PingResponse {
            transaction_id: vec![0x01, 0x02],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: PingResponsePayload,
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded, test_addr()).unwrap();
        assert!(decoded.is_response());
    }

    #[test]
    fn roundtrip_find_node_response() {
        let node = CompactNodeInfo {
            node_id: test_id(0xCD),
            addr: test_addr(),
        };
        let msg = DhtMessage::FindNodeResponse {
            transaction_id: vec![0x01],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: FindNodeResponsePayload { nodes: vec![node] },
        };
        let encoded = encode(&msg);
        let decoded =
            decode_response_with_method(&encoded, test_addr(), method::FIND_NODE).unwrap();
        if let DhtMessage::FindNodeResponse { payload, .. } = decoded {
            assert_eq!(payload.nodes.len(), 1);
        } else {
            panic!("expected FindNodeResponse");
        }
    }

    #[test]
    fn roundtrip_get_peers_response() {
        let node = CompactNodeInfo {
            node_id: test_id(0xCD),
            addr: test_addr(),
        };
        let peer = CompactPeerInfo { addr: test_addr() };
        let msg = DhtMessage::GetPeersResponse {
            transaction_id: vec![0x02],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: GetPeersResponsePayload {
                token: vec![0x42],
                nodes: vec![node],
                values: vec![peer],
            },
        };
        let encoded = encode(&msg);
        let decoded =
            decode_response_with_method(&encoded, test_addr(), method::GET_PEERS).unwrap();
        if let DhtMessage::GetPeersResponse { payload, .. } = decoded {
            assert_eq!(payload.token, vec![0x42]);
            assert_eq!(payload.nodes.len(), 1);
            assert_eq!(payload.values.len(), 1);
        } else {
            panic!("expected GetPeersResponse");
        }
    }

    #[test]
    fn roundtrip_announce_peer_response() {
        let msg = DhtMessage::AnnouncePeerResponse {
            transaction_id: vec![0x03],
            sender_id: test_id(0xAB),
            sender_addr: test_addr(),
            payload: AnnouncePeerResponsePayload,
        };
        let encoded = encode(&msg);
        let decoded =
            decode_response_with_method(&encoded, test_addr(), method::ANNOUNCE_PEER).unwrap();
        assert!(decoded.is_response());
    }

    #[test]
    fn roundtrip_error_message() {
        let msg = DhtMessage::Error {
            transaction_id: vec![0x01],
            sender_addr: test_addr(),
            code: 201,
            message: "some error".into(),
        };
        let encoded = encode(&msg);
        let decoded = decode(&encoded, test_addr()).unwrap();
        if let DhtMessage::Error { code, message, .. } = decoded {
            assert_eq!(code, 201);
            assert_eq!(message, "some error");
        } else {
            panic!("expected Error");
        }
    }

    // ── Error handling ─────────────────────────────────────────────────

    #[test]
    fn decode_empty_data() {
        assert!(decode(&[], test_addr()).is_err());
    }

    #[test]
    fn decode_invalid_bencode() {
        assert!(decode(b"xyz", test_addr()).is_err());
    }

    #[test]
    fn decode_missing_transaction_id() {
        let mut dict = BTreeMap::new();
        dict.insert(b"y".to_vec(), BencodeValue::Bytes(b"q".to_vec()));
        let encoded = BencodeValue::Dict(dict).encode();
        assert!(matches!(
            decode(&encoded, test_addr()),
            Err(MessageCodecError::MissingField(_))
        ));
    }

    #[test]
    fn decode_invalid_id_length() {
        let mut args = BTreeMap::new();
        args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0x01; 10]));
        let mut dict = BTreeMap::new();
        dict.insert(b"t".to_vec(), BencodeValue::Bytes(b"aa".to_vec()));
        dict.insert(b"y".to_vec(), BencodeValue::Bytes(b"q".to_vec()));
        dict.insert(b"q".to_vec(), BencodeValue::Bytes(b"ping".to_vec()));
        dict.insert(b"a".to_vec(), BencodeValue::Dict(args));
        let encoded = BencodeValue::Dict(dict).encode();
        assert!(matches!(
            decode(&encoded, test_addr()),
            Err(MessageCodecError::InvalidField { .. })
        ));
    }

    #[test]
    fn decode_unsupported_method() {
        let mut args = BTreeMap::new();
        args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xAB; ID_LENGTH]));
        let mut dict = BTreeMap::new();
        dict.insert(b"t".to_vec(), BencodeValue::Bytes(b"aa".to_vec()));
        dict.insert(b"y".to_vec(), BencodeValue::Bytes(b"q".to_vec()));
        dict.insert(b"q".to_vec(), BencodeValue::Bytes(b"unknown".to_vec()));
        dict.insert(b"a".to_vec(), BencodeValue::Dict(args));
        let encoded = BencodeValue::Dict(dict).encode();
        assert!(matches!(
            decode(&encoded, test_addr()),
            Err(MessageCodecError::UnsupportedMethod(_))
        ));
    }

    #[test]
    fn infer_method_from_response_structure() {
        // "nodes" key without "token" -> find_node
        let mut resp = BTreeMap::new();
        resp.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0; ID_LENGTH]));
        resp.insert(b"nodes".to_vec(), BencodeValue::Bytes(vec![0; 26]));
        let mut dict = BTreeMap::new();
        dict.insert(b"r".to_vec(), BencodeValue::Dict(resp));
        assert_eq!(infer_response_method(&dict), method::FIND_NODE);

        // "token" key -> get_peers
        let mut resp2 = BTreeMap::new();
        resp2.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0; ID_LENGTH]));
        resp2.insert(b"token".to_vec(), BencodeValue::Bytes(b"tok".to_vec()));
        let mut dict2 = BTreeMap::new();
        dict2.insert(b"r".to_vec(), BencodeValue::Dict(resp2));
        assert_eq!(infer_response_method(&dict2), method::GET_PEERS);

        // Only "id" -> ping
        let mut resp3 = BTreeMap::new();
        resp3.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0; ID_LENGTH]));
        let mut dict3 = BTreeMap::new();
        dict3.insert(b"r".to_vec(), BencodeValue::Dict(resp3));
        assert_eq!(infer_response_method(&dict3), method::PING);
    }
}
