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
use super::modern::{MutableValue, StoredItem};
use super::node::DhtNode;
use super::peer_storage::DhtPeerStorage;
use super::routing_table::RoutingTable;
use super::store::DhtItemStore;
use super::token_tracker::TokenTracker;
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
        self.handle_query_with_store(
            query,
            from,
            routing_table,
            token_tracker,
            peer_storage,
            None,
        )
    }

    pub fn handle_query_with_store(
        &self,
        query: &DhtMessage,
        from: SocketAddr,
        routing_table: &RoutingTable,
        token_tracker: &TokenTracker,
        peer_storage: &DhtPeerStorage,
        item_store: Option<&DhtItemStore>,
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
        let sender_id = query
            .a
            .as_ref()
            .and_then(|a| a.dict_get(b"id"))
            .and_then(|v| v.as_bytes())
            .and_then(|b| {
                if b.len() == 20 {
                    let mut id = [0u8; 20];
                    id.copy_from_slice(b);
                    Some(id)
                } else {
                    None
                }
            });

        if sender_id.is_none() {
            debug!(from = %from, "Ignoring DHT query with invalid node ID");
            return HandleResult {
                response: Some(DhtMessageBuilder::error_response(
                    &query.t,
                    203,
                    "Protocol Error",
                )),
                mark_good: false,
                sender_id: None,
            };
        }

        // aria2_original drops queries from its own local node before
        // dispatching them. Do the same here so a loopback packet cannot
        // create a response or reinsert the local ID into the routing table.
        if sender_id == Some(self.self_id) {
            debug!(from = %from, "Ignoring DHT query from local node");
            return HandleResult {
                response: None,
                mark_good: false,
                sender_id: None,
            };
        }

        trace!(
            method = %method,
            from = %from,
            sender_id = sender_id.map(hex::encode).as_deref().unwrap_or("?"),
            "Processing inbound DHT query"
        );

        let response = match method.as_str() {
            DhtQueryMethod::PING => self.handle_ping(&query.t, from, routing_table, token_tracker),
            DhtQueryMethod::FIND_NODE => {
                self.handle_find_node(&query.t, from, query, routing_table, token_tracker)
            }
            DhtQueryMethod::GET_PEERS => self.handle_get_peers(
                &query.t,
                from,
                query,
                routing_table,
                token_tracker,
                peer_storage,
            ),
            DhtQueryMethod::ANNOUNCE_PEER => {
                self.handle_announce_peer(&query.t, from, query, token_tracker, peer_storage)
            }
            DhtQueryMethod::GET if item_store.is_some() => self.handle_get_item(
                &query.t,
                from,
                query,
                routing_table,
                token_tracker,
                item_store.unwrap(),
            ),
            DhtQueryMethod::PUT if item_store.is_some() => {
                self.handle_put_item(&query.t, from, query, token_tracker, item_store.unwrap())
            }
            DhtQueryMethod::SAMPLE_INFOHASHES if item_store.is_some() => {
                self.handle_sample_infohashes(&query.t, query, routing_table, peer_storage)
            }
            _ => {
                debug!(method = %method, "Unknown DHT query method, sending error");
                Some(DhtMessageBuilder::error_response(
                    &query.t,
                    204,
                    "Method Unknown",
                ))
            }
        };

        HandleResult {
            response,
            mark_good: true,
            sender_id,
        }
    }

    fn handle_get_item(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        routing_table: &RoutingTable,
        token_tracker: &TokenTracker,
        item_store: &DhtItemStore,
    ) -> Option<DhtMessage> {
        let target: [u8; 20] = match query
            .a
            .as_ref()
            .and_then(|args| args.dict_get(b"target"))
            .and_then(|value| value.as_bytes())
            .and_then(|bytes| bytes.try_into().ok())
        {
            Some(target) => target,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let token = token_tracker.generate_token(&target, &from);
        let mut result = std::collections::BTreeMap::new();
        result.insert(b"id".to_vec(), BencodeValue::Bytes(self.self_id.to_vec()));
        result.insert(b"token".to_vec(), BencodeValue::Bytes(token.into_bytes()));
        if let Some(item) = item_store.get(&target) {
            let include = query
                .a
                .as_ref()
                .and_then(|args| args.dict_get(b"seq"))
                .and_then(|value| value.as_int())
                .is_none_or(|seq| match &item {
                    StoredItem::Mutable { item, .. } => item.sequence > seq,
                    StoredItem::Immutable { .. } => true,
                });
            if include {
                match item {
                    StoredItem::Immutable { value, .. } => {
                        result.insert(b"v".to_vec(), value);
                    }
                    StoredItem::Mutable { item, .. } => {
                        result.insert(b"k".to_vec(), BencodeValue::Bytes(item.public_key.to_vec()));
                        result.insert(
                            b"sig".to_vec(),
                            BencodeValue::Bytes(item.signature.to_vec()),
                        );
                        result.insert(b"seq".to_vec(), BencodeValue::Int(item.sequence));
                        result.insert(b"v".to_vec(), item.value);
                        if let Some(salt) = item.salt {
                            result.insert(b"salt".to_vec(), BencodeValue::Bytes(salt));
                        }
                    }
                }
            }
        }
        let nodes = Self::encode_compact_nodes(&routing_table.find_closest(&target, K));
        result.insert(b"nodes".to_vec(), BencodeValue::Bytes(nodes));
        Some(DhtMessage::new_response(
            tx.to_vec(),
            BencodeValue::Dict(result),
        ))
    }

    #[allow(clippy::needless_return)]
    fn handle_put_item(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        token_tracker: &TokenTracker,
        item_store: &DhtItemStore,
    ) -> Option<DhtMessage> {
        let args = match query.a.as_ref() {
            Some(args) => args,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let token = match args.dict_get(b"token").and_then(|v| v.as_bytes()) {
            Some(token) => token,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let value = match args.dict_get(b"v") {
            Some(value) => value.clone(),
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        let has_mutable_fields = args.dict_get(b"k").is_some()
            || args.dict_get(b"seq").is_some()
            || args.dict_get(b"sig").is_some()
            || args.dict_get(b"salt").is_some()
            || args.dict_get(b"cas").is_some();
        if let (Some(k), Some(seq), Some(sig)) = (
            args.dict_get(b"k").and_then(|v| v.as_bytes()),
            args.dict_get(b"seq").and_then(|v| v.as_int()),
            args.dict_get(b"sig").and_then(|v| v.as_bytes()),
        ) {
            let public_key: [u8; 32] = match k.try_into() {
                Ok(key) => key,
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        206,
                        "invalid signature",
                    ));
                }
            };
            let signature: [u8; 64] = match sig.try_into() {
                Ok(signature) => signature,
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        206,
                        "invalid signature",
                    ));
                }
            };
            let salt = match args.dict_get(b"salt") {
                Some(value) => match value.as_bytes() {
                    Some(salt) => Some(salt.to_vec()),
                    None => {
                        return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
                    }
                },
                None => None,
            };
            let target = StoredItem::mutable_target(&public_key, salt.as_deref());
            if !token_tracker.validate_token_bytes(token, &target, &from) {
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
            let item = MutableValue {
                public_key,
                signature,
                sequence: seq,
                salt,
                value,
            };
            match item_store.put_mutable(item, args.dict_get(b"cas").and_then(|v| v.as_int())) {
                Ok(_) => {
                    return Some(DhtMessage::new_response(
                        tx.to_vec(),
                        BencodeValue::Dict(std::collections::BTreeMap::from([(
                            b"id".to_vec(),
                            BencodeValue::Bytes(self.self_id.to_vec()),
                        )])),
                    ));
                }
                Err(super::store::StoreError::CasMismatch) => {
                    return Some(DhtMessageBuilder::error_response(tx, 301, "CAS mismatch"));
                }
                Err(super::store::StoreError::SequenceTooLow) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        302,
                        "sequence number less than current",
                    ));
                }
                Err(super::store::StoreError::ValueTooLarge) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        205,
                        "message too big",
                    ));
                }
                Err(super::store::StoreError::SaltTooLarge) => {
                    return Some(DhtMessageBuilder::error_response(tx, 207, "salt too big"));
                }
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        206,
                        "invalid signature",
                    ));
                }
            }
        } else if has_mutable_fields {
            return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
        } else {
            let target = StoredItem::immutable_target(&value);
            if !token_tracker.validate_token_bytes(token, &target, &from) {
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            }
            match item_store.put_immutable(value) {
                Ok(_) => Some(DhtMessage::new_response(
                    tx.to_vec(),
                    BencodeValue::Dict(std::collections::BTreeMap::from([(
                        b"id".to_vec(),
                        BencodeValue::Bytes(self.self_id.to_vec()),
                    )])),
                )),
                Err(super::store::StoreError::ValueTooLarge) => {
                    return Some(DhtMessageBuilder::error_response(
                        tx,
                        205,
                        "message too big",
                    ));
                }
                Err(_) => {
                    return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
                }
            }
        }
    }

    fn handle_sample_infohashes(
        &self,
        tx: &[u8],
        query: &DhtMessage,
        routing_table: &RoutingTable,
        peer_storage: &DhtPeerStorage,
    ) -> Option<DhtMessage> {
        let target: [u8; 20] = match query
            .a
            .as_ref()
            .and_then(|args| args.dict_get(b"target"))
            .and_then(|value| value.as_bytes())
            .and_then(|bytes| bytes.try_into().ok())
        {
            Some(target) => target,
            None => return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error")),
        };
        // BEP 51 samples the torrent swarm keyspace. BEP 44 item targets are
        // a separate keyspace and must not be advertised as torrent hashes.
        let mut info_hashes = peer_storage.info_hashes();
        info_hashes.sort_unstable();
        info_hashes.dedup();
        use rand::seq::SliceRandom;
        info_hashes.shuffle(&mut rand::thread_rng());
        let num = info_hashes.len();
        info_hashes.truncate(32);
        let mut result = std::collections::BTreeMap::new();
        result.insert(b"id".to_vec(), BencodeValue::Bytes(self.self_id.to_vec()));
        result.insert(b"interval".to_vec(), BencodeValue::Int(900));
        result.insert(b"num".to_vec(), BencodeValue::Int(num as i64));
        result.insert(
            b"samples".to_vec(),
            BencodeValue::Bytes(info_hashes.into_iter().flatten().collect()),
        );
        let closest = routing_table.find_closest(&target, K);
        let nodes = Self::encode_compact_nodes(&closest);
        let nodes6 = Self::encode_compact_nodes6(&closest);
        result.insert(b"nodes".to_vec(), BencodeValue::Bytes(nodes));
        if !nodes6.is_empty() {
            result.insert(b"nodes6".to_vec(), BencodeValue::Bytes(nodes6));
        }
        Some(DhtMessage::new_response(
            tx.to_vec(),
            BencodeValue::Dict(result),
        ))
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
        let target = query
            .a
            .as_ref()
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

        Some(DhtMessageBuilder::find_node_response(
            tx,
            &self.self_id,
            &compact_nodes,
        ))
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
        let info_hash_bytes = query
            .a
            .as_ref()
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
                tx,
                &self.self_id,
                &token_bytes,
                &peers,
            ))
        } else {
            // No peers known — return closest nodes instead
            let closest = routing_table.find_closest(&info_hash, K);
            let compact_nodes = Self::encode_compact_nodes(&closest);
            Some(DhtMessageBuilder::get_peers_response_with_nodes(
                tx,
                &self.self_id,
                &token_bytes,
                &compact_nodes,
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
        let port: u16 =
            if let Some(implied) = args.dict_get(b"implied_port").and_then(|v| v.as_int()) {
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
                SocketAddr::V6(v6) => SocketAddr::V6(std::net::SocketAddrV6::new(
                    *v6.ip(),
                    port,
                    v6.flowinfo(),
                    v6.scope_id(),
                )),
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
                SocketAddr::V6(_) => {}
            }
        }
        buf
    }

    fn encode_compact_nodes6(nodes: &[DhtNode]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(nodes.len() * 38);
        for node in nodes {
            let SocketAddr::V6(v6) = node.addr else {
                continue;
            };
            buf.extend_from_slice(&node.id);
            buf.extend_from_slice(&v6.ip().octets());
            buf.extend_from_slice(&v6.port().to_be_bytes());
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
    fn test_ignores_query_from_local_node() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();

        let query = DhtMessageBuilder::ping(1234, &[0xAAu8; 20]);
        let result = handler.handle_query(&query, "127.0.0.1:6881".parse().unwrap(), &rt, &tt, &ps);

        assert!(result.response.is_none());
        assert!(!result.mark_good);
        assert!(result.sender_id.is_none());
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
    fn test_sample_infohashes_counts_peer_store_keys() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();
        let info_hash = [0x44; 20];
        ps.add_peer(info_hash, "127.0.0.1:6881".parse().unwrap());
        let store = DhtItemStore::default();
        let query = super::super::modern::sample_infohashes_query(1, &[0xBB; 20], &info_hash);
        let result = handler.handle_query_with_store(
            &query,
            "10.0.0.1:6881".parse().unwrap(),
            &rt,
            &tt,
            &ps,
            Some(&store),
        );
        let response = result.response.unwrap();
        let body = response.r.unwrap();
        assert_eq!(
            body.dict_get(b"num").and_then(|value| value.as_int()),
            Some(1)
        );
        assert_eq!(
            body.dict_get(b"samples")
                .and_then(|value| value.as_bytes())
                .map(|value| value.len()),
            Some(20)
        );
    }

    #[test]
    fn test_put_rejects_non_bytes_salt() {
        use ed25519_dalek::{Signer, SigningKey};

        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();
        let store = DhtItemStore::default();
        let key = SigningKey::from_bytes(&[0x31; 32]);
        let mut item = MutableValue {
            public_key: key.verifying_key().to_bytes(),
            signature: [0u8; 64],
            sequence: 1,
            salt: None,
            value: BencodeValue::Bytes(b"value".to_vec()),
        };
        item.signature = key.sign(&item.signed_payload()).to_bytes();
        let target = StoredItem::mutable_target(&item.public_key, None);
        let from: SocketAddr = "10.0.0.1:6881".parse().unwrap();
        let token = tt.generate_token(&target, &from);
        let mut query =
            super::super::modern::put_query(7, &[0xBB; 20], token.as_bytes(), &item, None);
        if let Some(BencodeValue::Dict(args)) = query.a.as_mut() {
            args.insert(b"salt".to_vec(), BencodeValue::Int(1));
        } else {
            panic!("PUT query must have a dictionary of arguments");
        }

        let result = handler.handle_query_with_store(&query, from, &rt, &tt, &ps, Some(&store));
        assert!(result.response.is_some_and(|response| response.is_error()));
        assert!(store.get(&target).is_none());
    }

    #[test]
    fn test_invalid_sender_id_is_rejected_before_dispatch() {
        let handler = make_handler();
        let rt = make_routing_table();
        let tt = TokenTracker::new();
        let ps = DhtPeerStorage::new();
        let mut query = DhtMessageBuilder::ping(7, &[0x11; 20]);
        query.a = Some(BencodeValue::Dict(BTreeMap::new()));

        let result = handler.handle_query(&query, "127.0.0.1:6881".parse().unwrap(), &rt, &tt, &ps);
        assert!(!result.mark_good);
        assert!(result.sender_id.is_none());
        assert!(result.response.is_some_and(|response| response.is_error()));
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
        args.insert(
            b"info_hash".to_vec(),
            BencodeValue::Bytes(info_hash.to_vec()),
        );
        args.insert(b"port".to_vec(), BencodeValue::Int(5000));
        args.insert(
            b"token".to_vec(),
            BencodeValue::Bytes(token.as_bytes().to_vec()),
        );

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
        args.insert(
            b"info_hash".to_vec(),
            BencodeValue::Bytes(info_hash.to_vec()),
        );
        args.insert(b"port".to_vec(), BencodeValue::Int(5000));
        args.insert(
            b"token".to_vec(),
            BencodeValue::Bytes(b"bad_token".to_vec()),
        );

        let query = DhtMessage::new_query(1111, "announce_peer", BencodeValue::Dict(args));
        let result = handler.handle_query(&query, from, &rt, &tt, &ps);

        let resp = result.response.unwrap();
        assert!(resp.is_error());

        // Peer should NOT be stored
        let peers = ps.get_peers(&info_hash);
        assert!(peers.is_empty());
    }
}
