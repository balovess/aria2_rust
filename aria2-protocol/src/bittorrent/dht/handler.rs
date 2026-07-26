//! Inbound DHT query handler.
//!
//! Processes incoming KRPC query messages (ping, find_node, get_peers,
//! announce_peer) and generates appropriate responses. This corresponds to
//! the server-side logic in C++ `DHTMessageReceiver::receiveMessage` and
//! the per-message `doReceivedAction()` methods.

#[cfg(test)]
use std::collections::BTreeMap;
use std::net::SocketAddr;

use tracing::{debug, trace, warn};

use super::message::{DhtMessage, DhtMessageBuilder, DhtQueryMethod};
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::token_tracker::TokenTracker;
use super::peer_storage::DhtPeerStorage;

#[cfg(test)]
use crate::bittorrent::bencode::codec::BencodeValue;

/// Maximum number of closest nodes to return in find_node / get_peers responses.
const K: usize = 8;

/// Result of processing an inbound query.
pub struct HandleResult {
    /// The response message to send back (if any).
    pub response: Option<DhtMessage>,
    /// Whether the sender node should be marked as good and added to the routing table.
    pub mark_good: bool,
    /// The sender's node ID extracted from the query (for routing table insertion).
    pub sender_id: Option<[u8; 20]>,
}

/// Handles inbound DHT query messages and generates responses.
///
/// This is stateless — it takes references to the routing table, token tracker,
/// and peer storage to construct appropriate responses. Thread-safe because
/// it only reads from shared state (callers handle writes separately).
pub struct DhtQueryHandler {
    self_id: [u8; 20],
}

impl DhtQueryHandler {
    /// Create a new handler for the given local node ID.
    pub fn new(self_id: [u8; 20]) -> Self {
        Self { self_id }
    }

    /// Return the local node ID this handler is configured with.
    pub fn self_id(&self) -> [u8; 20] {
        self.self_id
    }

    /// Process an inbound DHT query message.
    ///
    /// Returns a `HandleResult` indicating what response to send and whether
    /// the sender should be marked as good in the routing table.
    pub fn handle_query(
        &self,
        query: &DhtMessage,
        from: SocketAddr,
        routing_table: &RoutingTable,
        token_tracker: &TokenTracker,
        peer_storage: &DhtPeerStorage,
    ) -> HandleResult {
        let method = match &query.q {
            Some(m) => &m.0,
            None => {
                warn!("DHT query with no method field, ignoring");
                return HandleResult {
                    response: None,
                    mark_good: false,
                    sender_id: None,
                };
            }
        };

        // Extract sender ID from query arguments
        let sender_id = query.a.as_ref().and_then(|a| a.dict_get(b"id")).and_then(|v| v.as_bytes()).and_then(|b| {
            if b.len() == 20 {
                let mut id = [0u8; 20];
                id.copy_from_slice(b);
                Some(id)
            } else {
                None
            }
        });

        trace!(
            method = %method,
            from = %from,
            sender_id = sender_id.map(|id| hex::encode(id)).as_deref().unwrap_or("?"),
            "Processing inbound DHT query"
        );

        let response = match method.as_str() {
            DhtQueryMethod::PING => self.handle_ping(&query.t, from, routing_table, token_tracker),
            DhtQueryMethod::FIND_NODE => self.handle_find_node(&query.t, from, query, routing_table, token_tracker),
            DhtQueryMethod::GET_PEERS => self.handle_get_peers(&query.t, from, query, routing_table, token_tracker, peer_storage),
            DhtQueryMethod::ANNOUNCE_PEER => self.handle_announce_peer(&query.t, from, query, token_tracker, peer_storage),
            _ => {
                debug!(method = %method, "Unknown DHT query method, sending error");
                Some(DhtMessageBuilder::error_response(&query.t, 204, "Method Unknown"))
            }
        };

        HandleResult {
            response,
            mark_good: true,
            sender_id,
        }
    }

    /// Handle a ping query: respond with our node ID.
    fn handle_ping(
        &self,
        tx: &[u8],
        _from: SocketAddr,
        _routing_table: &RoutingTable,
        _token_tracker: &TokenTracker,
    ) -> Option<DhtMessage> {
        Some(DhtMessageBuilder::ping_response(tx, &self.self_id))
    }

    /// Handle a find_node query: return K closest nodes to the target.
    fn handle_find_node(
        &self,
        tx: &[u8],
        _from: SocketAddr,
        query: &DhtMessage,
        routing_table: &RoutingTable,
        _token_tracker: &TokenTracker,
    ) -> Option<DhtMessage> {
        // Extract target ID from query
        let target = query.a.as_ref()
            .and_then(|a| a.dict_get(b"target"))
            .and_then(|v| v.as_bytes());

        let target_id: [u8; 20] = match target {
            Some(b) if b.len() == 20 => {
                let mut id = [0u8; 20];
                id.copy_from_slice(b);
                id
            }
            _ => {
                debug!("find_node query with invalid/missing target, sending error");
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
        };

        let closest = routing_table.find_closest(&target_id, K);
        let compact_nodes = Self::encode_compact_nodes(&closest);

        Some(DhtMessageBuilder::find_node_response(tx, &self.self_id, &compact_nodes))
    }

    /// Handle a get_peers query: return peers if known, otherwise closest nodes.
    fn handle_get_peers(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        routing_table: &RoutingTable,
        token_tracker: &TokenTracker,
        peer_storage: &DhtPeerStorage,
    ) -> Option<DhtMessage> {
        // Extract info_hash from query
        let info_hash_bytes = query.a.as_ref()
            .and_then(|a| a.dict_get(b"info_hash"))
            .and_then(|v| v.as_bytes());

        let info_hash: [u8; 20] = match info_hash_bytes {
            Some(b) if b.len() == 20 => {
                let mut id = [0u8; 20];
                id.copy_from_slice(b);
                id
            }
            _ => {
                debug!("get_peers query with invalid/missing info_hash, sending error");
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
        };

        // Generate a token for this (info_hash, from) pair
        let token = token_tracker.generate_token(&info_hash, &from);
        let token_bytes = token.as_bytes().to_vec();

        // Check if we know any peers for this info_hash
        let peers = peer_storage.get_peers(&info_hash);

        if !peers.is_empty() {
            Some(DhtMessageBuilder::get_peers_response_with_peers(
                tx, &self.self_id, &token_bytes, &peers,
            ))
        } else {
            // No peers known — return closest nodes instead
            let closest = routing_table.find_closest(&info_hash, K);
            let compact_nodes = Self::encode_compact_nodes(&closest);
            Some(DhtMessageBuilder::get_peers_response_with_nodes(
                tx, &self.self_id, &token_bytes, &compact_nodes,
            ))
        }
    }

    /// Handle an announce_peer query: validate token and store the peer.
    fn handle_announce_peer(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        token_tracker: &TokenTracker,
        peer_storage: &DhtPeerStorage,
    ) -> Option<DhtMessage> {
        let args = match &query.a {
            Some(a) => a,
            None => {
                debug!("announce_peer query with no arguments");
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
        };

        // Extract info_hash
        let info_hash: [u8; 20] = match args.dict_get(b"info_hash").and_then(|v| v.as_bytes()) {
            Some(b) if b.len() == 20 => {
                let mut id = [0u8; 20];
                id.copy_from_slice(b);
                id
            }
            _ => {
                debug!("announce_peer with invalid/missing info_hash");
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
        };

        // Extract and validate token
        let token = match args.dict_get(b"token").and_then(|v| v.as_str()) {
            Some(t) => t,
            None => {
                debug!("announce_peer with missing token");
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
        };

        if !token_tracker.validate_token(token, &info_hash, &from) {
            debug!(from = %from, "announce_peer with invalid token");
            return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
        }

        // Extract port — if "implied_port" is set, use the source port
        let port: u16 = if let Some(implied) = args.dict_get(b"implied_port").and_then(|v| v.as_int()) {
            if implied != 0 {
                from.port()
            } else {
                args.dict_get(b"port").and_then(|v| v.as_int()).unwrap_or(0) as u16
            }
        } else {
            args.dict_get(b"port").and_then(|v| v.as_int()).unwrap_or(0) as u16
        };

        if port > 0 {
            let peer_addr: SocketAddr = match from {
                SocketAddr::V4(v4) => SocketAddr::V4(std::net::SocketAddrV4::new(*v4.ip(), port)),
                SocketAddr::V6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(*v6.ip(), port, v6.flowinfo(), v6.scope_id())),
            };
            peer_storage.add_peer(info_hash, peer_addr);
            trace!(
                info_hash = %hex::encode(info_hash),
                peer = %peer_addr,
                "Stored announced peer"
            );
        }

        Some(DhtMessageBuilder::announce_peer_response(tx, &self.self_id))
    }

    /// Encode a list of DHT nodes into BEP 0005 compact node format.
    ///
    /// IPv4 only: 20 bytes node ID + 4 bytes IP + 2 bytes port = 26 bytes per node.
    fn encode_compact_nodes(nodes: &[DhtNode]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(nodes.len() * 26);
        for node in nodes {
            buf.extend_from_slice(&node.id);
            match node.addr {
                SocketAddr::V4(v4) => {
                    buf.extend_from_slice(&v4.ip().octets());
                    buf.extend_from_slice(&v4.port().to_be_bytes());
                }
                SocketAddr::V6(v6) => {
                    buf.extend_from_slice(&v6.ip().octets());
                    buf.extend_from_slice(&v6.port().to_be_bytes());
                }
            }
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bittorrent::dht::routing_table::RoutingTable;

    fn make_handler() -> DhtQueryHandler {
        DhtQueryHandler::new([0xAAu8; 20])
    }

    fn make_routing_table() -> RoutingTable {
        let mut rt = RoutingTable::new([0xAAu8; 20]);
        // Add some nodes
        for i in 0..8u8 {
            let node = DhtNode::new([i; 20], format!("10.0.0.{}:6881", i).parse().unwrap());
            rt.insert(node);
        }
        rt
    }

    #[test]
    fn test_handle_ping() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        let query = DhtMessageBuilder::ping(1234, &[0xBBu8; 20]);
        let result = handler.handle_query(&query, "10.0.0.1:6881".parse().unwrap(), &rt, &tt, &ps);

        assert!(result.mark_good);
        assert!(result.response.is_some());
        let resp = result.response.unwrap();
        assert!(resp.is_response());
        assert_eq!(result.sender_id, Some([0xBBu8; 20]));
    }

    #[test]
    fn test_handle_find_node() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        let query = DhtMessageBuilder::find_node(5678, &[0xBBu8; 20], &[0x05u8; 20]);
        let result = handler.handle_query(&query, "10.0.0.1:6881".parse().unwrap(), &rt, &tt, &ps);

        assert!(result.response.is_some());
        let resp = result.response.unwrap();
        assert!(resp.is_response());

        // Should contain nodes in the response
        let r = resp.r.as_ref().unwrap();
        let nodes = r.dict_get(b"nodes").and_then(|v| v.as_bytes());
        assert!(nodes.is_some());
        // 8 nodes × 26 bytes each for IPv4
        assert_eq!(nodes.unwrap().len(), 8 * 26);
    }

    #[test]
    fn test_handle_get_peers_no_peers() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        let query = DhtMessageBuilder::get_peers(9999, &[0xBBu8; 20], &[0xCCu8; 20]);
        let result = handler.handle_query(&query, "10.0.0.1:6881".parse().unwrap(), &rt, &tt, &ps);

        let resp = result.response.unwrap();
        let r = resp.r.as_ref().unwrap();
        // Should have nodes (no peers known)
        assert!(r.dict_get(b"nodes").is_some());
        // Should have a token
        assert!(r.dict_get(b"token").is_some());
    }

    #[test]
    fn test_handle_get_peers_with_peers() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        // Pre-populate peer storage
        let info_hash = [0xCCu8; 20];
        ps.add_peer(info_hash, "192.168.1.1:5000".parse().unwrap());

        let query = DhtMessageBuilder::get_peers(9999, &[0xBBu8; 20], &info_hash);
        let result = handler.handle_query(&query, "10.0.0.1:6881".parse().unwrap(), &rt, &tt, &ps);

        let resp = result.response.unwrap();
        let r = resp.r.as_ref().unwrap();
        // Should have values (peers known)
        assert!(r.dict_get(b"values").is_some());
        // Should NOT have nodes
        assert!(r.dict_get(b"nodes").is_none());
    }

    #[test]
    fn test_handle_announce_peer_valid_token() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        let info_hash = [0xDDu8; 20];
        let from: SocketAddr = "10.0.0.99:6881".parse().unwrap();

        // Generate a valid token
        let token = tt.generate_token(&info_hash, &from);

        // Build announce_peer query manually with the valid token
        let mut args = BTreeMap::new();
        args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xBBu8; 20]));
        args.insert(b"info_hash".to_vec(), BencodeValue::Bytes(info_hash.to_vec()));
        args.insert(b"port".to_vec(), BencodeValue::Int(5000));
        args.insert(b"token".to_vec(), BencodeValue::Bytes(token.as_bytes().to_vec()));

        let query = DhtMessage::new_query(1111, "announce_peer", BencodeValue::Dict(args));
        let result = handler.handle_query(&query, from, &rt, &tt, &ps);

        assert!(result.response.is_some());
        let resp = result.response.unwrap();
        assert!(resp.is_response());

        // Peer should be stored
        let peers = ps.get_peers(&info_hash);
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].port(), 5000);
    }

    #[test]
    fn test_handle_announce_peer_invalid_token() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        let info_hash = [0xEEu8; 20];
        let from: SocketAddr = "10.0.0.99:6881".parse().unwrap();

        let mut args = BTreeMap::new();
        args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xBBu8; 20]));
        args.insert(b"info_hash".to_vec(), BencodeValue::Bytes(info_hash.to_vec()));
        args.insert(b"port".to_vec(), BencodeValue::Int(5000));
        args.insert(b"token".to_vec(), BencodeValue::Bytes(b"bad_token".to_vec()));

        let query = DhtMessage::new_query(1111, "announce_peer", BencodeValue::Dict(args));
        let result = handler.handle_query(&query, from, &rt, &tt, &ps);

        let resp = result.response.unwrap();
        assert!(resp.is_error());

        // Peer should NOT be stored
        let peers = ps.get_peers(&info_hash);
        assert!(peers.is_empty());
    }
}
