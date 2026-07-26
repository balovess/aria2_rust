//! Iterative DHT lookup with alpha-parallelism (BEP 0005 Kademlia).
//!
//! Implements the core iterative lookup algorithm used for both `find_node`
//! and `get_peers` queries. The C++ implementation uses a template class
//! `DHTAbstractNodeLookupTask` with callbacks; this Rust version uses
//! async/await with oneshot channels for a simpler, more idiomatic design.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, warn};

use super::client::extract_compact_nodes_from_response;
use super::client::extract_compact_peers_from_response;
use super::message::{DhtMessage, DhtMessageBuilder};
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::tracker::{QueryType, TransactionTracker};

/// Kademlia K-constant: max nodes to track in a lookup.
const K: usize = 8;
/// Kademlia ALPHA-constant: parallel in-flight queries.
const ALPHA: usize = 3;
/// Per-query timeout for responses.
const QUERY_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum number of rounds before giving up.
const MAX_ROUNDS: usize = 20;

/// Entry in the lookup's tracked node list.
#[derive(Debug, Clone)]
struct LookupEntry {
    node_id: [u8; 20],
    addr: SocketAddr,
    /// Whether this node has already been queried.
    used: bool,
}

impl LookupEntry {
    #[allow(dead_code)]
    fn distance_to(&self, target: &[u8; 20]) -> [u8; 20] {
        let mut dist = [0u8; 20];
        for i in 0..20 {
            dist[i] = self.node_id[i] ^ target[i];
        }
        dist
    }
}

/// Result of a `find_node` iterative lookup.
#[derive(Debug, Clone)]
pub struct NodeLookupResult {
    /// K closest nodes found to the target ID.
    pub closest_nodes: Vec<DhtNode>,
    /// Number of nodes contacted during the lookup.
    pub nodes_contacted: usize,
}

/// Result of a `get_peers` iterative lookup.
#[derive(Debug, Clone)]
pub struct PeerLookupResult {
    /// Discovered peer addresses serving the requested info hash.
    pub peers: Vec<SocketAddr>,
    /// K closest nodes that returned a token (for subsequent announce).
    pub token_nodes: Vec<(SocketAddr, [u8; 20], Vec<u8>)>,
    /// Number of nodes contacted during the lookup.
    pub nodes_contacted: usize,
}

/// Performs an iterative `find_node` lookup for the given target ID.
///
/// This follows the standard Kademlia iterative lookup algorithm (BEP 0005):
/// 1. Start with K closest nodes from the local routing table.
/// 2. Send up to ALPHA queries in parallel to the closest unused nodes.
/// 3. On response, add newly discovered nodes, re-sort by distance.
/// 4. Send more queries to the closest unused nodes.
/// 5. Terminate when all in-flight queries resolve and no new nodes to query.
pub async fn iterative_find_node(
    target: &[u8; 20],
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
) -> NodeLookupResult {
    let mut entries = initialize_entries(target, routing_table).await;
    let mut in_flight = 0usize;
    let mut nodes_contacted = 0usize;
    let mut rounds = 0usize;

    // Send initial batch
    in_flight = send_batch(
        target,
        self_id,
        &mut entries,
        in_flight,
        socket,
        tracker,
        QueryType::FindNode,
    )
    .await;

    while in_flight > 0 && rounds < MAX_ROUNDS {
        rounds += 1;

        // Wait for any one response
        match tokio::time::timeout(QUERY_TIMEOUT, recv_response(socket)).await {
            Ok(Some((response, from))) => {
                in_flight = in_flight.saturating_sub(1);
                nodes_contacted += 1;

                // Mark the responding node as good
                mark_node_good(&from, routing_table).await;

                // Extract nodes from the response and add to entries
                let new_nodes = extract_compact_nodes_from_response(&response);
                for (addr, nid) in new_nodes {
                    let new_node = DhtNode::new(nid, addr);
                    add_node_to_table(routing_table, new_node.clone()).await;
                    insert_entry(&mut entries, nid, addr, target);
                }

                // Sort entries by distance and dedup
                sort_and_dedup(&mut entries, target);
            }
            Ok(None) => {
                // Decoded message failed — skip
            }
            Err(_) => {
                // Timeout on recv — break if no in-flight remain
                in_flight = in_flight.saturating_sub(1);
            }
        }

        // Send more queries if possible
        in_flight = send_batch(
            target,
            self_id,
            &mut entries,
            in_flight,
            socket,
            tracker,
            QueryType::FindNode,
        )
        .await;

        if in_flight == 0 {
            break;
        }
    }

    // Collect K closest nodes from entries
    let closest_nodes: Vec<DhtNode> = entries
        .iter()
        .take(K)
        .map(|e| DhtNode::new(e.node_id, e.addr))
        .collect();

    NodeLookupResult {
        closest_nodes,
        nodes_contacted,
    }
}

/// Performs an iterative `get_peers` lookup for the given info hash.
///
/// Same algorithm as `find_node`, but also collects peer addresses and
/// tokens from responses. Tokens are needed for subsequent `announce_peer`.
pub async fn iterative_get_peers(
    info_hash: &[u8; 20],
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
) -> PeerLookupResult {
    let mut entries = initialize_entries(info_hash, routing_table).await;
    let mut in_flight = 0usize;
    let mut nodes_contacted = 0usize;
    let mut all_peers: Vec<SocketAddr> = Vec::new();
    let mut token_nodes: Vec<(SocketAddr, [u8; 20], Vec<u8>)> = Vec::new();
    let mut rounds = 0usize;

    in_flight = send_batch(
        info_hash,
        self_id,
        &mut entries,
        in_flight,
        socket,
        tracker,
        QueryType::GetPeers,
    )
    .await;

    while in_flight > 0 && rounds < MAX_ROUNDS {
        rounds += 1;

        match tokio::time::timeout(QUERY_TIMEOUT, recv_response(socket)).await {
            Ok(Some((response, from))) => {
                in_flight = in_flight.saturating_sub(1);
                nodes_contacted += 1;

                mark_node_good(&from, routing_table).await;

                // Extract peers
                let peers = extract_compact_peers_from_response(&response);
                if !peers.is_empty() {
                    all_peers.extend(peers);
                }

                // Extract token from response (for subsequent announce)
                if let Some(r) = &response.r {
                    if let Some(token_val) = r.dict_get(b"token").and_then(|v| v.as_bytes()) {
                        // Find the node ID for this address
                        let node_id = entries
                            .iter()
                            .find(|e| e.addr == from)
                            .map(|e| e.node_id)
                            .unwrap_or([0u8; 20]);
                        token_nodes.push((from, node_id, token_val.to_vec()));
                    }
                }

                // Extract nodes from response
                let new_nodes = extract_compact_nodes_from_response(&response);
                for (addr, nid) in new_nodes {
                    let new_node = DhtNode::new(nid, addr);
                    add_node_to_table(routing_table, new_node).await;
                    insert_entry(&mut entries, nid, addr, info_hash);
                }

                sort_and_dedup(&mut entries, info_hash);
            }
            Ok(None) => {}
            Err(_) => {
                in_flight = in_flight.saturating_sub(1);
            }
        }

        in_flight = send_batch(
            info_hash,
            self_id,
            &mut entries,
            in_flight,
            socket,
            tracker,
            QueryType::GetPeers,
        )
        .await;

        if in_flight == 0 {
            break;
        }
    }

    // Dedup peers
    all_peers.sort();
    all_peers.dedup();

    // Keep only K closest token_nodes
    token_nodes.sort_by(|a, b| {
        let dist_a = xor_distance(&a.1, info_hash);
        let dist_b = xor_distance(&b.1, info_hash);
        dist_a.cmp(&dist_b)
    });
    token_nodes.truncate(K);

    PeerLookupResult {
        peers: all_peers,
        token_nodes,
        nodes_contacted,
    }
}

/// Send announce_peer to nodes that provided tokens.
///
/// After a successful `get_peers` lookup, this sends `announce_peer`
/// queries to up to K closest nodes that returned a token.
pub async fn announce_to_token_nodes(
    info_hash: &[u8; 20],
    self_id: &[u8; 20],
    port: u16,
    token_nodes: &[(SocketAddr, [u8; 20], Vec<u8>)],
    socket: &DhtSocket,
    _tracker: &Arc<TransactionTracker>,
) -> usize {
    let mut announced = 0usize;

    for (addr, _node_id, token) in token_nodes.iter().take(K) {
        let token_str = match std::str::from_utf8(token) {
            Ok(s) => s.to_string(),
            Err(_) => hex::encode(token),
        };
        let msg = DhtMessageBuilder::announce_peer(0, self_id, info_hash, port, &token_str);
        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to encode announce_peer: {}", e);
                continue;
            }
        };

        if let Err(e) = socket.send_to(*addr, &encoded).await {
            debug!(addr = %addr, "Failed to send announce_peer: {}", e);
            continue;
        }
        announced += 1;
    }

    announced
}

// ==================== Internal helpers ====================

/// Initialize lookup entries from the K closest nodes in the routing table.
async fn initialize_entries(
    target: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
) -> Vec<LookupEntry> {
    let rt = routing_table.read().await;
    rt.find_closest(target, K)
        .into_iter()
        .map(|n| LookupEntry {
            node_id: n.id,
            addr: n.addr,
            used: false,
        })
        .collect()
}

/// Send up to ALPHA queries to the closest unused entries.
///
/// Returns the new in-flight count.
async fn send_batch(
    target: &[u8; 20],
    self_id: &[u8; 20],
    entries: &mut [LookupEntry],
    current_in_flight: usize,
    socket: &DhtSocket,
    _tracker: &Arc<TransactionTracker>,
    query_type: QueryType,
) -> usize {
    let to_send = ALPHA.saturating_sub(current_in_flight);
    if to_send == 0 {
        return current_in_flight;
    }

    let mut sent = 0usize;
    for entry in entries.iter_mut() {
        if entry.used {
            continue;
        }
        if sent >= to_send {
            break;
        }

        let msg = match query_type {
            QueryType::FindNode => DhtMessageBuilder::find_node(0, self_id, target),
            QueryType::GetPeers => DhtMessageBuilder::get_peers(0, self_id, target),
            _ => continue,
        };

        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(_) => continue,
        };

        if socket.send_to(entry.addr, &encoded).await.is_ok() {
            entry.used = true;
            sent += 1;
        }
    }

    current_in_flight + sent
}

/// Receive a single UDP message and decode it.
async fn recv_response(socket: &DhtSocket) -> Option<(DhtMessage, SocketAddr)> {
    let mut buf = [0u8; 4096];
    match socket
        .recv_with_timeout(&mut buf, Duration::from_secs(5))
        .await
    {
        Ok((len, from)) if len > 0 => match DhtMessage::decode(&buf[..len]) {
            Ok(msg) => Some((msg, from)),
            Err(_) => None,
        },
        _ => None,
    }
}

/// Insert a new entry into the lookup list, avoiding duplicates.
fn insert_entry(
    entries: &mut Vec<LookupEntry>,
    node_id: [u8; 20],
    addr: SocketAddr,
    target: &[u8; 20],
) {
    // Skip if already present
    if entries.iter().any(|e| e.node_id == node_id) {
        return;
    }

    entries.push(LookupEntry {
        node_id,
        addr,
        used: false,
    });

    // Truncate to K entries if exceeded, keeping closest
    if entries.len() > K * 2 {
        sort_and_dedup(entries, target);
        entries.truncate(K * 2);
    }
}

/// Sort entries by XOR distance to target and remove duplicates.
fn sort_and_dedup(entries: &mut Vec<LookupEntry>, target: &[u8; 20]) {
    entries.sort_by(|a, b| {
        let da = xor_distance(&a.node_id, target);
        let db = xor_distance(&b.node_id, target);
        da.cmp(&db)
    });

    // Dedup by node_id
    let mut seen = HashSet::new();
    entries.retain(|e| seen.insert(e.node_id));
}

/// Compute XOR distance as a comparable byte array.
fn xor_distance(a: &[u8; 20], b: &[u8; 20]) -> [u8; 20] {
    let mut dist = [0u8; 20];
    for i in 0..20 {
        dist[i] = a[i] ^ b[i];
    }
    dist
}

/// Mark a node as good in the routing table.
///
/// This is currently a no-op because the RoutingTable doesn't have a
/// find_by_addr method. Nodes get marked good through the
/// TransactionTracker when they respond to our tracked queries.
async fn mark_node_good(
    _addr: &SocketAddr,
    _routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
) {
    // No-op: see comment above
}

/// Add a discovered node to the routing table.
async fn add_node_to_table(routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>, node: DhtNode) {
    let mut rt = routing_table.write().await;
    rt.insert(node);
}
