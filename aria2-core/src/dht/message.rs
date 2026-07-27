//! DHT message types for the BitTorrent mainline DHT protocol (BEP 5).
//!
//! Defines all query, response, and error message types used in Kademlia DHT
//! communication. Each message carries a transaction ID, sender node ID,
//! sender address, and message-specific payload.
//!
//! Wire format (BEP 5):
//! - Query:    `{"t": txn_id, "y": "q", "q": method, "a": {args}}`
//! - Response: `{"t": txn_id, "y": "r", "r": {return_values}}`
//! - Error:    `{"t": txn_id, "y": "e", "e": [code, msg]}`
//!
//! C++ reference: DHTMessage.h + derived query/response classes.

use std::fmt;
use std::net::SocketAddr;

use super::constants::ID_LENGTH;
use super::node_id::NodeId;

// ── DHT method names (BEP 5) ──────────────────────────────────────────────

/// DHT method name constants matching the C++ static strings.
pub mod method {
    pub const PING: &str = "ping";
    pub const FIND_NODE: &str = "find_node";
    pub const GET_PEERS: &str = "get_peers";
    pub const ANNOUNCE_PEER: &str = "announce_peer";
}

// ── Bencode dictionary key names ──────────────────────────────────────────

/// Bencode dictionary key constants used in DHT messages.
pub mod key {
    pub const T: &str = "t";
    pub const Y: &str = "y";
    pub const Q: &str = "q";
    pub const A: &str = "a";
    pub const R: &str = "r";
    pub const E: &str = "e";
    pub const V: &str = "v";
    pub const ID: &str = "id";
    pub const TARGET: &str = "target";
    pub const INFO_HASH: &str = "info_hash";
    pub const TOKEN: &str = "token";
    pub const PORT: &str = "port";
    pub const NODES: &str = "nodes";
    pub const NODES6: &str = "nodes6";
    pub const VALUES: &str = "values";
}

// ── Message type discriminant ─────────────────────────────────────────────

/// Discriminant for the three DHT message categories.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageTypeKind {
    Query,
    Response,
    Error,
}

// ── Compact node / peer info ──────────────────────────────────────────────

/// Compact node info: 20-byte node ID + network address.
///
/// Wire format (IPv4): 26 bytes = 20 (ID) + 4 (IP) + 2 (port)
/// Wire format (IPv6): 38 bytes = 20 (ID) + 16 (IP) + 2 (port)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactNodeInfo {
    pub node_id: NodeId,
    pub addr: SocketAddr,
}

impl CompactNodeInfo {
    /// Pack into compact wire format bytes.
    pub fn pack(&self) -> Option<Vec<u8>> {
        use std::net::IpAddr;
        let mut buf = Vec::with_capacity(38);
        buf.extend_from_slice(self.node_id.as_bytes());
        match self.addr.ip() {
            IpAddr::V4(v4) => {
                buf.extend_from_slice(&v4.octets());
                buf.extend_from_slice(&self.addr.port().to_be_bytes());
            }
            IpAddr::V6(v6) => {
                buf.extend_from_slice(&v6.octets());
                buf.extend_from_slice(&self.addr.port().to_be_bytes());
            }
        }
        Some(buf)
    }

    /// Unpack compact node info from wire-format bytes.
    ///
    /// Tries IPv4 (26 bytes/unit) first, then IPv6 (38 bytes).
    /// Returns all successfully parsed entries.
    pub fn unpack_all(data: &[u8]) -> Vec<CompactNodeInfo> {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

        let mut nodes = Vec::new();
        let v4_unit = ID_LENGTH + 4 + 2; // 26
        let v6_unit = ID_LENGTH + 16 + 2; // 38

        let (unit, is_v6) = if data.len() % v4_unit == 0 && !data.is_empty() {
            (v4_unit, false)
        } else if data.len() % v6_unit == 0 && !data.is_empty() {
            (v6_unit, true)
        } else {
            return nodes;
        };

        for chunk in data.chunks_exact(unit) {
            let node_id = NodeId::from_slice(&chunk[..ID_LENGTH]);
            let addr = if is_v6 {
                let mut ip_bytes = [0u8; 16];
                ip_bytes.copy_from_slice(&chunk[ID_LENGTH..ID_LENGTH + 16]);
                let port = u16::from_be_bytes([chunk[ID_LENGTH + 16], chunk[ID_LENGTH + 17]]);
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port)
            } else {
                let mut ip_bytes = [0u8; 4];
                ip_bytes.copy_from_slice(&chunk[ID_LENGTH..ID_LENGTH + 4]);
                let port = u16::from_be_bytes([chunk[ID_LENGTH + 4], chunk[ID_LENGTH + 5]]);
                SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip_bytes)), port)
            };
            nodes.push(CompactNodeInfo { node_id, addr });
        }
        nodes
    }
}

/// Compact peer info: IP address + port (no node ID).
///
/// Wire format (IPv4): 6 bytes = 4 (IP) + 2 (port)
/// Wire format (IPv6): 18 bytes = 16 (IP) + 2 (port)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompactPeerInfo {
    pub addr: SocketAddr,
}

impl CompactPeerInfo {
    /// Pack into compact wire format bytes.
    pub fn pack(&self) -> Vec<u8> {
        use std::net::IpAddr;
        match self.addr.ip() {
            IpAddr::V4(v4) => {
                let mut buf = Vec::with_capacity(6);
                buf.extend_from_slice(&v4.octets());
                buf.extend_from_slice(&self.addr.port().to_be_bytes());
                buf
            }
            IpAddr::V6(v6) => {
                let mut buf = Vec::with_capacity(18);
                buf.extend_from_slice(&v6.octets());
                buf.extend_from_slice(&self.addr.port().to_be_bytes());
                buf
            }
        }
    }

    /// Unpack compact peer info. Returns `None` if not IPv4 (6) or IPv6 (18).
    pub fn unpack(data: &[u8]) -> Option<CompactPeerInfo> {
        use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
        match data.len() {
            6 => {
                let mut ip_bytes = [0u8; 4];
                ip_bytes.copy_from_slice(&data[..4]);
                let port = u16::from_be_bytes([data[4], data[5]]);
                Some(CompactPeerInfo {
                    addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip_bytes)), port),
                })
            }
            18 => {
                let mut ip_bytes = [0u8; 16];
                ip_bytes.copy_from_slice(&data[..16]);
                let port = u16::from_be_bytes([data[16], data[17]]);
                Some(CompactPeerInfo {
                    addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port),
                })
            }
            _ => None,
        }
    }
}

// ── Query message payloads ────────────────────────────────────────────────

/// Payload for a ping query. C++: DHTPingMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PingQueryPayload;

/// Payload for a find_node query. C++: DHTFindNodeMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindNodeQueryPayload {
    pub target: NodeId,
}

/// Payload for a get_peers query. C++: DHTGetPeersMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetPeersQueryPayload {
    pub info_hash: NodeId,
}

/// Payload for an announce_peer query. C++: DHTAnnouncePeerMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnouncePeerQueryPayload {
    pub info_hash: NodeId,
    pub port: u16,
    pub token: Vec<u8>,
}

// ── Response message payloads ─────────────────────────────────────────────

/// Payload for a ping response. C++: DHTPingReplyMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PingResponsePayload;

/// Payload for a find_node response. C++: DHTFindNodeReplyMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindNodeResponsePayload {
    pub nodes: Vec<CompactNodeInfo>,
}

/// Payload for a get_peers response. C++: DHTGetPeersReplyMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GetPeersResponsePayload {
    pub token: Vec<u8>,
    pub nodes: Vec<CompactNodeInfo>,
    pub values: Vec<CompactPeerInfo>,
}

/// Payload for an announce_peer response. C++: DHTAnnouncePeerReplyMessage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnnouncePeerResponsePayload;

// ── Top-level DHT message enum ────────────────────────────────────────────

/// A DHT message following BEP 5 wire format.
///
/// Each variant carries: transaction_id, sender NodeId, sender SocketAddr,
/// and message-type-specific payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DhtMessage {
    // ── Queries (y = "q") ─────────────────────────────────────────────
    PingQuery {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: PingQueryPayload,
    },
    FindNodeQuery {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: FindNodeQueryPayload,
    },
    GetPeersQuery {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: GetPeersQueryPayload,
    },
    AnnouncePeerQuery {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: AnnouncePeerQueryPayload,
    },

    // ── Responses (y = "r") ───────────────────────────────────────────
    PingResponse {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: PingResponsePayload,
    },
    FindNodeResponse {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: FindNodeResponsePayload,
    },
    GetPeersResponse {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: GetPeersResponsePayload,
    },
    AnnouncePeerResponse {
        transaction_id: Vec<u8>,
        sender_id: NodeId,
        sender_addr: SocketAddr,
        payload: AnnouncePeerResponsePayload,
    },

    // ── Error (y = "e") ───────────────────────────────────────────────
    Error {
        transaction_id: Vec<u8>,
        sender_addr: SocketAddr,
        code: i64,
        message: String,
    },
}

impl DhtMessage {
    /// Return the transaction ID bytes.
    pub fn transaction_id(&self) -> &[u8] {
        match self {
            DhtMessage::PingQuery { transaction_id, .. }
            | DhtMessage::FindNodeQuery { transaction_id, .. }
            | DhtMessage::GetPeersQuery { transaction_id, .. }
            | DhtMessage::AnnouncePeerQuery { transaction_id, .. }
            | DhtMessage::PingResponse { transaction_id, .. }
            | DhtMessage::FindNodeResponse { transaction_id, .. }
            | DhtMessage::GetPeersResponse { transaction_id, .. }
            | DhtMessage::AnnouncePeerResponse { transaction_id, .. }
            | DhtMessage::Error { transaction_id, .. } => transaction_id,
        }
    }

    /// Return the sender node ID, if present (error messages lack one).
    pub fn sender_id(&self) -> Option<&NodeId> {
        match self {
            DhtMessage::PingQuery { sender_id, .. }
            | DhtMessage::FindNodeQuery { sender_id, .. }
            | DhtMessage::GetPeersQuery { sender_id, .. }
            | DhtMessage::AnnouncePeerQuery { sender_id, .. }
            | DhtMessage::PingResponse { sender_id, .. }
            | DhtMessage::FindNodeResponse { sender_id, .. }
            | DhtMessage::GetPeersResponse { sender_id, .. }
            | DhtMessage::AnnouncePeerResponse { sender_id, .. } => Some(sender_id),
            DhtMessage::Error { .. } => None,
        }
    }

    /// Return the sender address.
    pub fn sender_addr(&self) -> &SocketAddr {
        match self {
            DhtMessage::PingQuery { sender_addr, .. }
            | DhtMessage::FindNodeQuery { sender_addr, .. }
            | DhtMessage::GetPeersQuery { sender_addr, .. }
            | DhtMessage::AnnouncePeerQuery { sender_addr, .. }
            | DhtMessage::PingResponse { sender_addr, .. }
            | DhtMessage::FindNodeResponse { sender_addr, .. }
            | DhtMessage::GetPeersResponse { sender_addr, .. }
            | DhtMessage::AnnouncePeerResponse { sender_addr, .. }
            | DhtMessage::Error { sender_addr, .. } => sender_addr,
        }
    }

    /// Return the message type category.
    pub fn kind(&self) -> MessageTypeKind {
        match self {
            DhtMessage::PingQuery { .. }
            | DhtMessage::FindNodeQuery { .. }
            | DhtMessage::GetPeersQuery { .. }
            | DhtMessage::AnnouncePeerQuery { .. } => MessageTypeKind::Query,
            DhtMessage::PingResponse { .. }
            | DhtMessage::FindNodeResponse { .. }
            | DhtMessage::GetPeersResponse { .. }
            | DhtMessage::AnnouncePeerResponse { .. } => MessageTypeKind::Response,
            DhtMessage::Error { .. } => MessageTypeKind::Error,
        }
    }

    /// Return true if this is a query message.
    pub fn is_query(&self) -> bool {
        self.kind() == MessageTypeKind::Query
    }

    /// Return true if this is a response message.
    pub fn is_response(&self) -> bool {
        self.kind() == MessageTypeKind::Response
    }

    /// Return true if this is an error message.
    pub fn is_error(&self) -> bool {
        self.kind() == MessageTypeKind::Error
    }

    /// Return the DHT method name. Returns `None` for error messages.
    pub fn method_name(&self) -> Option<&'static str> {
        match self {
            DhtMessage::PingQuery { .. } | DhtMessage::PingResponse { .. } => Some(method::PING),
            DhtMessage::FindNodeQuery { .. } | DhtMessage::FindNodeResponse { .. } => {
                Some(method::FIND_NODE)
            }
            DhtMessage::GetPeersQuery { .. } | DhtMessage::GetPeersResponse { .. } => {
                Some(method::GET_PEERS)
            }
            DhtMessage::AnnouncePeerQuery { .. } | DhtMessage::AnnouncePeerResponse { .. } => {
                Some(method::ANNOUNCE_PEER)
            }
            DhtMessage::Error { .. } => None,
        }
    }
}

impl fmt::Display for DhtMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let txn_hex = hex::encode(self.transaction_id());
        match self {
            DhtMessage::PingQuery {
                sender_id,
                sender_addr,
                ..
            } => write!(
                f,
                "DHT ping query txn={} from {} ({})",
                txn_hex, sender_id, sender_addr
            ),
            DhtMessage::FindNodeQuery {
                sender_id,
                sender_addr,
                payload,
                ..
            } => write!(
                f,
                "DHT find_node query txn={} from {} ({}) target={}",
                txn_hex, sender_id, sender_addr, payload.target
            ),
            DhtMessage::GetPeersQuery {
                sender_id,
                sender_addr,
                payload,
                ..
            } => write!(
                f,
                "DHT get_peers query txn={} from {} ({}) info_hash={}",
                txn_hex, sender_id, sender_addr, payload.info_hash
            ),
            DhtMessage::AnnouncePeerQuery {
                sender_id,
                sender_addr,
                payload,
                ..
            } => write!(
                f,
                "DHT announce_peer query txn={} from {} ({}) info_hash={} port={}",
                txn_hex, sender_id, sender_addr, payload.info_hash, payload.port
            ),
            DhtMessage::PingResponse {
                sender_id,
                sender_addr,
                ..
            } => write!(
                f,
                "DHT ping response txn={} from {} ({})",
                txn_hex, sender_id, sender_addr
            ),
            DhtMessage::FindNodeResponse {
                sender_id,
                sender_addr,
                payload,
                ..
            } => write!(
                f,
                "DHT find_node response txn={} from {} ({}) nodes={}",
                txn_hex,
                sender_id,
                sender_addr,
                payload.nodes.len()
            ),
            DhtMessage::GetPeersResponse {
                sender_id,
                sender_addr,
                payload,
                ..
            } => write!(
                f,
                "DHT get_peers response txn={} from {} ({}) nodes={} values={}",
                txn_hex,
                sender_id,
                sender_addr,
                payload.nodes.len(),
                payload.values.len()
            ),
            DhtMessage::AnnouncePeerResponse {
                sender_id,
                sender_addr,
                ..
            } => write!(
                f,
                "DHT announce_peer response txn={} from {} ({})",
                txn_hex, sender_id, sender_addr
            ),
            DhtMessage::Error {
                sender_addr,
                code,
                message,
                ..
            } => write!(
                f,
                "DHT error txn={} from {} code={} msg={}",
                txn_hex, sender_addr, code, message
            ),
        }
    }
}

// ── DHT version string ────────────────────────────────────────────────────

/// Build the 4-byte DHT version string matching C++ format.
///
/// C++: `getDefaultVersion()` — produces "A2" + 2-byte big-endian version.
pub fn make_version_string(version: u16) -> Vec<u8> {
    use super::constants::DHT_VERSION;
    let v = if version == 0 { DHT_VERSION } else { version };
    let vbytes = v.to_be_bytes();
    vec![b'A', b'2', vbytes[0], vbytes[1]]
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn addr_v4() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 6881)
    }

    fn addr_v6() -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 1)), 6881)
    }

    #[test]
    fn compact_node_info_roundtrip_ipv4() {
        let info = CompactNodeInfo {
            node_id: NodeId::from_slice(&[0xAB; ID_LENGTH]),
            addr: addr_v4(),
        };
        let packed = info.pack().unwrap();
        assert_eq!(packed.len(), 26);
        let unpacked = CompactNodeInfo::unpack_all(&packed);
        assert_eq!(unpacked.len(), 1);
        assert_eq!(unpacked[0], info);
    }

    #[test]
    fn compact_node_info_roundtrip_ipv6() {
        let info = CompactNodeInfo {
            node_id: NodeId::from_slice(&[0xCD; ID_LENGTH]),
            addr: addr_v6(),
        };
        let packed = info.pack().unwrap();
        assert_eq!(packed.len(), 38);
        let unpacked = CompactNodeInfo::unpack_all(&packed);
        assert_eq!(unpacked[0], info);
    }

    #[test]
    fn compact_peer_info_roundtrip_ipv4() {
        let info = CompactPeerInfo { addr: addr_v4() };
        assert_eq!(CompactPeerInfo::unpack(&info.pack()).unwrap(), info);
    }

    #[test]
    fn compact_peer_info_roundtrip_ipv6() {
        let info = CompactPeerInfo { addr: addr_v6() };
        assert_eq!(CompactPeerInfo::unpack(&info.pack()).unwrap(), info);
    }

    #[test]
    fn compact_peer_info_unpack_invalid_length() {
        assert!(CompactPeerInfo::unpack(&[0; 5]).is_none());
        assert!(CompactPeerInfo::unpack(&[0; 7]).is_none());
    }

    #[test]
    fn message_accessors() {
        let msg = DhtMessage::PingQuery {
            transaction_id: vec![0x01, 0x02],
            sender_id: NodeId::ZERO,
            sender_addr: addr_v4(),
            payload: PingQueryPayload,
        };
        assert_eq!(msg.transaction_id(), &[0x01, 0x02]);
        assert_eq!(msg.sender_id(), Some(&NodeId::ZERO));
        assert!(msg.is_query());
        assert_eq!(msg.method_name(), Some(method::PING));
    }

    #[test]
    fn message_kind_error() {
        let msg = DhtMessage::Error {
            transaction_id: vec![0x01],
            sender_addr: addr_v4(),
            code: 201,
            message: "err".into(),
        };
        assert!(msg.is_error());
        assert!(msg.sender_id().is_none());
    }

    #[test]
    fn version_string() {
        assert_eq!(make_version_string(3), vec![b'A', b'2', 0x00, 0x03]);
    }

    #[test]
    fn display_format() {
        let msg = DhtMessage::FindNodeQuery {
            transaction_id: vec![0xAA],
            sender_id: NodeId::ZERO,
            sender_addr: addr_v4(),
            payload: FindNodeQueryPayload {
                target: NodeId::MAX,
            },
        };
        let s = format!("{}", msg);
        assert!(s.contains("find_node query") && s.contains("target="));
    }

    #[test]
    fn multiple_compact_nodes_roundtrip() {
        let nodes: Vec<CompactNodeInfo> = (0..3)
            .map(|i| {
                let mut id = [0u8; ID_LENGTH];
                id[0] = i;
                CompactNodeInfo {
                    node_id: NodeId(id),
                    addr: SocketAddr::new(
                        IpAddr::V4(Ipv4Addr::new(10, 0, 0, i + 1)),
                        6881 + i as u16,
                    ),
                }
            })
            .collect();
        let packed: Vec<u8> = nodes.iter().flat_map(|n| n.pack().unwrap()).collect();
        assert_eq!(packed.len(), 3 * 26);
        assert_eq!(CompactNodeInfo::unpack_all(&packed), nodes);
    }
}
