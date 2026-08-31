//! Iterative DHT lookup with alpha-parallelism (BEP 0005 Kademlia).
//!
//! Implements the core iterative lookup algorithm used for both `find_node`
//! and `get_peers` queries. The C++ implementation uses a template class
//! `DHTAbstractNodeLookupTask` with callbacks; this Rust version uses
//! async/await with oneshot channels for a simpler, more idiomatic design.

use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tracing::{debug, warn};

use super::client::extract_compact_nodes_from_response;
use super::client::extract_compact_peers_from_response;
use super::message::{DhtMessage, DhtMessageBuilder};
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::tracker::{QueryType, TrackedResponse, TransactionTracker};

/// Kademlia K-constant: max nodes to track in a lookup.
const K: usize = 8;
/// Kademlia ALPHA-constant: parallel in-flight queries.
const ALPHA: usize = 3;
/// Maximum number of rounds before giving up.
const MAX_ROUNDS: usize = 20;

struct LookupResponse {
    response: Option<TrackedResponse>,
    node_id: [u8; 20],
}

type LookupPendingResponse = Pin<Box<dyn Future<Output = LookupResponse> + Send>>;

/// Entry in the lookup's tracked node list.
#[derive(Debug, Clone)]
struct LookupEntry {
    node_id: [u8; 20],
    addr: SocketAddr,
    /// Whether this node has already been queried.
    used: bool,
}

struct LookupRequest<'a> {
    target: &'a [u8; 20],
    self_id: &'a [u8; 20],
    info_hash: Option<[u8; 20]>,
    socket: &'a DhtSocket,
    tracker: &'a Arc<TransactionTracker>,
    query_type: QueryType,
    query_timeout: Duration,
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
    query_timeout: Duration,
) -> NodeLookupResult {
    let mut entries = initialize_entries(target, routing_table).await;
    let mut pending = FuturesUnordered::<LookupPendingResponse>::new();
    let mut nodes_contacted = 0usize;
    let mut rounds = 0usize;
    let request = LookupRequest {
        target,
        self_id,
        info_hash: None,
        socket,
        tracker,
        query_type: QueryType::FindNode,
        query_timeout,
    };

    // Send initial batch
    send_batch(&request, &mut entries, &mut pending).await;

    while !pending.is_empty() && rounds < MAX_ROUNDS {
        rounds += 1;

        // The engine receive loop is the sole UDP reader. Each lookup waits
        // on its own tracked transaction, so concurrent lookups cannot steal
        // one another's responses.
        if let Some(result) = pending.next().await {
            if let Some(response) = result.response {
                let from = response.from;
                let message = response.message;
                nodes_contacted += 1;

                // Mark the responding node as good
                mark_node_good(&from, &message, routing_table).await;

                // Extract nodes from the response and add to entries
                let new_nodes = extract_compact_nodes_from_response(&message);
                for (addr, nid) in new_nodes {
                    let new_node = DhtNode::new(nid, addr);
                    add_node_to_table(routing_table, new_node).await;
                    insert_entry(&mut entries, nid, addr, target);
                }

                // Sort entries by distance and dedup
                sort_and_dedup(&mut entries, target);
            } else {
                mark_node_bad(&result.node_id, routing_table).await;
            }
        }

        // Send more queries if possible
        send_batch(&request, &mut entries, &mut pending).await;
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
    query_timeout: Duration,
) -> PeerLookupResult {
    let mut entries = initialize_entries(info_hash, routing_table).await;
    let mut pending = FuturesUnordered::<LookupPendingResponse>::new();
    let mut nodes_contacted = 0usize;
    let mut all_peers: Vec<SocketAddr> = Vec::new();
    let mut token_nodes: Vec<(SocketAddr, [u8; 20], Vec<u8>)> = Vec::new();
    let mut rounds = 0usize;
    let request = LookupRequest {
        target: info_hash,
        self_id,
        info_hash: Some(*info_hash),
        socket,
        tracker,
        query_type: QueryType::GetPeers,
        query_timeout,
    };

    send_batch(&request, &mut entries, &mut pending).await;

    while !pending.is_empty() && rounds < MAX_ROUNDS {
        rounds += 1;

        if let Some(result) = pending.next().await {
            if let Some(response) = result.response {
                let from = response.from;
                let message = response.message;
                nodes_contacted += 1;

                mark_node_good(&from, &message, routing_table).await;

                // Extract peers
                let peers = extract_compact_peers_from_response(&message);
                if !peers.is_empty() {
                    all_peers.extend(peers);
                }

                // Extract token from response (for subsequent announce)
                if let Some(r) = &message.r
                    && let Some(token_val) = r.dict_get(b"token").and_then(|v| v.as_bytes())
                {
                    // Find the node ID for this address
                    let node_id = entries
                        .iter()
                        .find(|e| e.addr == from)
                        .map(|e| e.node_id)
                        .unwrap_or([0u8; 20]);
                    token_nodes.push((from, node_id, token_val.to_vec()));
                }

                // Extract nodes from response
                let new_nodes = extract_compact_nodes_from_response(&message);
                for (addr, nid) in new_nodes {
                    let new_node = DhtNode::new(nid, addr);
                    add_node_to_table(routing_table, new_node).await;
                    insert_entry(&mut entries, nid, addr, info_hash);
                }

                sort_and_dedup(&mut entries, info_hash);
            } else {
                mark_node_bad(&result.node_id, routing_table).await;
            }
        }

        send_batch(&request, &mut entries, &mut pending).await;
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
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
) -> usize {
    let mut sends = FuturesUnordered::new();

    for (addr, node_id, token) in token_nodes.iter().take(K) {
        let token_str = match std::str::from_utf8(token) {
            Ok(s) => s.to_string(),
            Err(_) => hex::encode(token),
        };

        let (transaction_id, response_wait) = tracker.allocate_wait(
            QueryType::AnnouncePeer,
            *addr,
            Some(*node_id),
            Some(*info_hash),
            query_timeout,
        );
        let msg =
            DhtMessageBuilder::announce_peer(transaction_id, self_id, info_hash, port, &token_str);
        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(e) => {
                warn!("Failed to encode announce_peer: {}", e);
                continue;
            }
        };

        let socket = socket.clone();
        let addr = *addr;
        sends.push(async move {
            if let Err(e) = socket.send_to(addr, &encoded).await {
                debug!(addr = %addr, "Failed to send announce_peer: {}", e);
                return false;
            }
            let _ = response_wait.wait(query_timeout).await;
            true
        });
    }

    let mut announced = 0usize;
    while let Some(sent) = sends.next().await {
        if sent {
            announced += 1;
        }
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
/// Register and send up to `ALPHA` queries, adding their response waits to the
/// lookup's pending set.
async fn send_batch(
    request: &LookupRequest<'_>,
    entries: &mut [LookupEntry],
    pending: &mut FuturesUnordered<LookupPendingResponse>,
) {
    let to_send = ALPHA.saturating_sub(pending.len());
    if to_send == 0 {
        return;
    }

    let mut sends = FuturesUnordered::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        if entry.used {
            continue;
        }
        if sends.len() >= to_send {
            break;
        }

        let (transaction_id, response_wait) = request.tracker.allocate_wait(
            request.query_type,
            entry.addr,
            Some(entry.node_id),
            request.info_hash,
            request.query_timeout,
        );
        let query_timeout = request.query_timeout;
        let node_id = entry.node_id;
        let wait_for_response = Box::pin(async move {
            LookupResponse {
                response: response_wait.wait(query_timeout).await,
                node_id,
            }
        });

        let msg = match request.query_type {
            QueryType::FindNode => {
                DhtMessageBuilder::find_node(transaction_id, request.self_id, request.target)
            }
            QueryType::GetPeers => {
                DhtMessageBuilder::get_peers(transaction_id, request.self_id, request.target)
            }
            _ => unreachable!("lookup batches only contain find_node or get_peers"),
        };

        let encoded = match msg.encode() {
            Ok(e) => e,
            Err(_) => {
                continue;
            }
        };

        let socket = request.socket.clone();
        let addr = entry.addr;
        sends.push(async move {
            if socket.send_to(addr, &encoded).await.is_ok() {
                Some((entry_index, wait_for_response))
            } else {
                None
            }
        });
    }

    while let Some(result) = sends.next().await {
        if let Some((entry_index, wait_for_response)) = result {
            entries[entry_index].used = true;
            pending.push(wait_for_response);
        }
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
async fn mark_node_good(
    addr: &SocketAddr,
    message: &DhtMessage,
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
) {
    let Some(node_id) = message
        .r
        .as_ref()
        .and_then(|result| result.dict_get(b"id"))
        .and_then(|id| id.as_bytes())
        .filter(|id| id.len() == 20)
        .map(|id| {
            let mut node_id = [0u8; 20];
            node_id.copy_from_slice(id);
            node_id
        })
    else {
        return;
    };

    let mut rt = routing_table.write().await;
    rt.mark_good(&node_id);
    rt.insert(DhtNode::new(node_id, *addr));
}

/// Mark a node as failed after its tracked lookup expires.
async fn mark_node_bad(node_id: &[u8; 20], routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>) {
    let mut rt = routing_table.write().await;
    rt.mark_bad(node_id);
}

/// Add a discovered node to the routing table.
async fn add_node_to_table(routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>, node: DhtNode) {
    let mut rt = routing_table.write().await;
    rt.insert(node);
}
