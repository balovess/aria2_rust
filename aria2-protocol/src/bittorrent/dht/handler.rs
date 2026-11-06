//! Inbound DHT query handler.
//!
//! Validates and dispatches incoming KRPC queries. BEP 5 peer queries,
//! BEP 44 item storage, and BEP 51 sampling are handled by focused modules.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use tracing::{debug, trace, warn};

use super::message::{DhtMessage, DhtMessageBuilder, DhtQueryMethod};
use super::node::DhtNode;
use super::peer_storage::DhtPeerStorage;
use super::routing_table::RoutingTable;
use super::store::DhtItemStore;
use super::token_tracker::TokenTracker;

mod items;
mod sample;
#[cfg(test)]
mod tests;

/// Maximum number of closest nodes to return in find_node / get_peers responses.
const K: usize = 8;
/// Keep `get_peers` values below the UDP payload size used by the original
/// implementation to avoid path-MTU fragmentation.
const MAX_PEERS_IN_GET_PEERS_RESPONSE: usize = 25;

/// A core-owned lookup for an active local torrent peer advertised by DHT.
pub type DhtLocalPeerLookup = dyn Fn(&[u8; 20], IpAddr) -> Option<SocketAddr> + Send + Sync;

/// Result of processing an inbound query.
pub struct HandleResult {
    /// The response message to send back (if any).
    pub response: Option<DhtMessage>,
    /// Sender ID eligible for promotion after a successful query response.
    pub sender_to_promote: Option<[u8; 20]>,
}

/// Handles inbound DHT query messages and generates responses.
///
/// The handler does not mutate DHT state. An optional local-peer lookup adds
/// active torrent endpoints to `get_peers` replies; callers supply that
/// application-owned lookup without coupling this protocol crate to the core.
pub struct DhtQueryHandler {
    self_id: [u8; 20],
    local_peer_lookup: Option<Arc<DhtLocalPeerLookup>>,
}

impl DhtQueryHandler {
    /// Create a new handler for the given local node ID.
    pub fn new(self_id: [u8; 20]) -> Self {
        Self {
            self_id,
            local_peer_lookup: None,
        }
    }

    /// Attach the active-download lookup used to advertise this client in
    /// inbound `get_peers` replies.
    pub fn with_local_peer_lookup(mut self, lookup: Arc<DhtLocalPeerLookup>) -> Self {
        self.local_peer_lookup = Some(lookup);
        self
    }

    /// Return the local node ID this handler is configured with.
    pub fn self_id(&self) -> [u8; 20] {
        self.self_id
    }

    /// Process an inbound DHT query message.
    ///
    /// Returns the response and any sender eligible for routing-table
    /// promotion. `item_store` enables BEP 44 get/put handling; BEP 51
    /// sampling uses peer storage only.
    pub fn handle_query(
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
                    sender_to_promote: None,
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
                sender_to_promote: None,
            };
        }

        // aria2_original drops queries from its own local node before
        // dispatching them. Do the same here so a loopback packet cannot
        // create a response or reinsert the local ID into the routing table.
        if sender_id == Some(self.self_id) {
            debug!(from = %from, "Ignoring DHT query from local node");
            return HandleResult {
                response: None,
                sender_to_promote: None,
            };
        }

        trace!(
            method = %method,
            from = %from,
            sender_id = sender_id.map(hex::encode).as_deref().unwrap_or("?"),
            "Processing inbound DHT query"
        );

        let unknown_method = || {
            debug!(method = %method, "Unknown DHT query method, sending error");
            Some(DhtMessageBuilder::error_response(
                &query.t,
                204,
                "Method Unknown",
            ))
        };

        let response = match method.as_str() {
            DhtQueryMethod::PING => self.handle_ping(&query.t),
            DhtQueryMethod::FIND_NODE => {
                self.handle_find_node(&query.t, from, query, routing_table)
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
            DhtQueryMethod::GET => match item_store {
                Some(store) => {
                    self.handle_get_item(&query.t, from, query, routing_table, token_tracker, store)
                }
                None => unknown_method(),
            },
            DhtQueryMethod::PUT => match item_store {
                Some(store) => self.handle_put_item(&query.t, from, query, token_tracker, store),
                None => unknown_method(),
            },
            DhtQueryMethod::SAMPLE_INFOHASHES => {
                self.handle_sample_infohashes(&query.t, query, routing_table, peer_storage)
            }
            _ => unknown_method(),
        };

        let sender_to_promote = if response.as_ref().is_some_and(DhtMessage::is_response) {
            sender_id
        } else {
            None
        };
        HandleResult {
            response,
            sender_to_promote,
        }
    }

    /// Handle a ping query: respond with our node ID.
    fn handle_ping(&self, tx: &[u8]) -> Option<DhtMessage> {
        Some(DhtMessageBuilder::ping_response(tx, &self.self_id))
    }

    /// Handle a find_node query: return K closest nodes to the target.
    fn handle_find_node(
        &self,
        tx: &[u8],
        from: SocketAddr,
        query: &DhtMessage,
        routing_table: &RoutingTable,
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
        if from.is_ipv6() {
            let compact_nodes = Self::encode_compact_nodes6(&closest);
            Some(DhtMessageBuilder::find_node_response6(
                tx,
                &self.self_id,
                &compact_nodes,
            ))
        } else {
            let compact_nodes = Self::encode_compact_nodes(&closest);
            Some(DhtMessageBuilder::find_node_response(
                tx,
                &self.self_id,
                &compact_nodes,
            ))
        }
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
        let mut peers = peer_storage.get_peers(&info_hash);
        peers.retain(|peer| peer.is_ipv4() == from.is_ipv4());
        let local_peer = self
            .local_peer_lookup
            .as_deref()
            .and_then(|lookup| lookup(&info_hash, from.ip()))
            .filter(|peer| peer.is_ipv4() == from.is_ipv4());
        if let Some(local_peer) = local_peer.filter(|peer| !peers.contains(peer)) {
            peers.truncate(MAX_PEERS_IN_GET_PEERS_RESPONSE - 1);
            peers.push(local_peer);
        } else {
            peers.truncate(MAX_PEERS_IN_GET_PEERS_RESPONSE);
        }

        if !peers.is_empty() {
            let closest = routing_table.find_closest(&info_hash, K);
            let compact_nodes = if from.is_ipv6() {
                Self::encode_compact_nodes6(&closest)
            } else {
                Self::encode_compact_nodes(&closest)
            };
            if compact_nodes.is_empty() {
                Some(DhtMessageBuilder::get_peers_response_with_peers(
                    tx,
                    &self.self_id,
                    &token_bytes,
                    &peers,
                ))
            } else if from.is_ipv6() {
                Some(DhtMessageBuilder::get_peers_response_with_peers_and_nodes6(
                    tx,
                    &self.self_id,
                    &token_bytes,
                    &peers,
                    &compact_nodes,
                ))
            } else {
                Some(DhtMessageBuilder::get_peers_response_with_peers_and_nodes(
                    tx,
                    &self.self_id,
                    &token_bytes,
                    &peers,
                    &compact_nodes,
                ))
            }
        } else {
            // No peers known — return closest nodes instead
            let closest = routing_table.find_closest(&info_hash, K);
            if from.is_ipv6() {
                let compact_nodes = Self::encode_compact_nodes6(&closest);
                Some(DhtMessageBuilder::get_peers_response_with_nodes6(
                    tx,
                    &self.self_id,
                    &token_bytes,
                    &compact_nodes,
                ))
            } else {
                let compact_nodes = Self::encode_compact_nodes(&closest);
                Some(DhtMessageBuilder::get_peers_response_with_nodes(
                    tx,
                    &self.self_id,
                    &token_bytes,
                    &compact_nodes,
                ))
            }
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

        // Extract port — if "implied_port" is set, use the source port.
        let port = if args
            .dict_get(b"implied_port")
            .and_then(|value| value.as_int())
            .is_some_and(|implied| implied != 0)
        {
            from.port()
        } else {
            let Some(port) = args
                .dict_get(b"port")
                .and_then(|value| value.as_int())
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port != 0)
            else {
                debug!(from = %from, "announce_peer with invalid/missing port");
                return Some(DhtMessageBuilder::error_response(tx, 203, "Protocol Error"));
            };
            port
        };

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
