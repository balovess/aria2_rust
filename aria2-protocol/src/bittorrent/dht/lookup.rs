//! Shared state and Kademlia routing helpers for iterative DHT lookups.

mod announce;
mod item;
mod node;
mod peer;
mod sample;

pub use announce::announce_to_token_nodes;
pub(super) use announce::announce_to_token_nodes_and_update_routing_table;
pub use item::{iterative_get_item, iterative_get_item_for_publish};
pub use node::iterative_find_node;
pub use peer::iterative_get_peers;
pub use sample::iterative_sample_infohashes;

use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use super::message::{DhtMessage, DhtMessageBuilder};
use super::modern::{
    MutableValue, SampleInfoHashesResponse, StoredItem, get_query, sample_infohashes_query,
};
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::tracker::{QueryType, TrackedResponse, TransactionTracker};

/// Kademlia K-constant: maximum nodes retained for a lookup.
pub(super) const K: usize = 8;
/// Kademlia ALPHA-constant: maximum concurrent queries.
const ALPHA: usize = 3;
/// Maximum lookup rounds before giving up.
pub(super) const MAX_ROUNDS: usize = 20;

pub(super) struct LookupResponse {
    response: Option<TrackedResponse>,
    node_id: [u8; 20],
}

pub(super) type LookupPendingResponse = Pin<Box<dyn Future<Output = LookupResponse> + Send>>;

/// Candidate node and whether this lookup has already queried it.
#[derive(Debug, Clone)]
pub(super) struct LookupEntry {
    pub(super) node_id: [u8; 20],
    pub(super) addr: SocketAddr,
    used: bool,
}

pub(super) struct LookupRequest<'a> {
    pub(super) target: &'a [u8; 20],
    pub(super) self_id: &'a [u8; 20],
    pub(super) socket: &'a DhtSocket,
    pub(super) tracker: &'a Arc<TransactionTracker>,
    pub(super) query_type: QueryType,
    pub(super) query_timeout: Duration,
}

/// Result of a `find_node` iterative lookup.
#[derive(Debug, Clone)]
pub struct NodeLookupResult {
    /// K closest nodes found to the target ID.
    pub closest_nodes: Vec<DhtNode>,
    /// Number of queries accepted by the local UDP socket (whether or not nodes reply).
    pub nodes_contacted: usize,
}

/// Result of a `get_peers` iterative lookup.
#[derive(Debug, Clone)]
pub struct PeerLookupResult {
    /// Discovered peer addresses serving the requested info hash.
    pub peers: Vec<SocketAddr>,
    /// K closest nodes that returned a token for `announce_peer`.
    pub token_nodes: Vec<(SocketAddr, [u8; 20], Vec<u8>)>,
    /// Number of queries accepted by the local UDP socket (whether or not nodes reply).
    pub nodes_contacted: usize,
}

#[derive(Debug, Clone)]
pub struct ItemLookupResult {
    pub item: Option<StoredItem>,
    pub token_nodes: Vec<(SocketAddr, [u8; 20], Vec<u8>)>,
    /// Number of queries accepted by the local UDP socket (whether or not nodes reply).
    pub nodes_contacted: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SampleLookupResult {
    pub response: Option<SampleInfoHashesResponse>,
    /// Number of queries accepted by the local UDP socket (whether or not nodes reply).
    pub nodes_contacted: usize,
}

/// Initialize lookup entries with the K closest nodes from the routing table.
pub(super) async fn initialize_entries(
    target: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    self_id: &[u8; 20],
) -> Vec<LookupEntry> {
    let rt = routing_table.read().await;
    rt.find_closest(target, K)
        .into_iter()
        .filter(|node| &node.id != self_id)
        .map(|node| LookupEntry {
            node_id: node.id,
            addr: node.addr,
            used: false,
        })
        .collect()
}

/// Send up to ALPHA queries to the closest unused entries.
pub(super) async fn send_batch_with_seq(
    request: &LookupRequest<'_>,
    seq: Option<i64>,
    entries: &mut [LookupEntry],
    pending: &mut FuturesUnordered<LookupPendingResponse>,
) -> usize {
    let to_send = ALPHA.saturating_sub(pending.len());
    if to_send == 0 {
        return 0;
    }

    let mut sends = FuturesUnordered::new();
    for (entry_index, entry) in entries.iter().enumerate() {
        if entry.used {
            continue;
        }
        if sends.len() >= to_send {
            break;
        }

        let (transaction_id, response_wait) =
            request
                .tracker
                .allocate_wait(request.query_type, entry.addr, request.query_timeout);
        let node_id = entry.node_id;
        let wait_for_response = Box::pin(async move {
            LookupResponse {
                response: response_wait.wait().await,
                node_id,
            }
        });

        let message = match request.query_type {
            QueryType::FindNode => {
                DhtMessageBuilder::find_node(transaction_id, request.self_id, request.target)
            }
            QueryType::GetPeers => {
                DhtMessageBuilder::get_peers(transaction_id, request.self_id, request.target)
            }
            QueryType::Get => get_query(transaction_id, request.self_id, request.target, seq),
            QueryType::SampleInfohashes => {
                sample_infohashes_query(transaction_id, request.self_id, request.target)
            }
            _ => unreachable!("unsupported lookup query type"),
        };

        let encoded = message.encode();

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

    let mut sent_queries = 0;
    while let Some(result) = sends.next().await {
        if let Some((entry_index, wait_for_response)) = result {
            sent_queries += 1;
            entries[entry_index].used = true;
            pending.push(wait_for_response);
        }
    }
    sent_queries
}

pub(super) fn result_node_id(message: &DhtMessage) -> Option<[u8; 20]> {
    message
        .r
        .as_ref()
        .and_then(|result| result.dict_get(b"id"))
        .and_then(|value| value.as_bytes())
        .and_then(|bytes| bytes.try_into().ok())
}

pub(super) fn parse_stored_item(
    target: &[u8; 20],
    result: &crate::bittorrent::bencode::codec::BencodeValue,
) -> Option<StoredItem> {
    let value = result.dict_get(b"v")?.clone();
    if let (Some(key), Some(signature), Some(sequence)) = (
        result.dict_get(b"k").and_then(|value| value.as_bytes()),
        result.dict_get(b"sig").and_then(|value| value.as_bytes()),
        result.dict_get(b"seq").and_then(|value| value.as_int()),
    ) {
        Some(StoredItem::Mutable {
            target: *target,
            item: MutableValue {
                public_key: key.try_into().ok()?,
                signature: signature.try_into().ok()?,
                sequence,
                salt: match result.dict_get(b"salt") {
                    Some(value) => Some(value.as_bytes()?.to_vec()),
                    None => None,
                },
                value,
            },
        })
    } else {
        Some(StoredItem::Immutable {
            target: *target,
            value,
        })
    }
}

/// Add a discovered node unless it is self or already present.
pub(super) fn insert_entry(
    entries: &mut Vec<LookupEntry>,
    node_id: [u8; 20],
    addr: SocketAddr,
    target: &[u8; 20],
    self_id: &[u8; 20],
) {
    if &node_id == self_id || entries.iter().any(|entry| entry.node_id == node_id) {
        return;
    }

    entries.push(LookupEntry {
        node_id,
        addr,
        used: false,
    });
    if entries.len() > K * 2 {
        sort_and_dedup(entries, target);
        entries.truncate(K * 2);
    }
}

pub(super) fn sort_and_dedup(entries: &mut Vec<LookupEntry>, target: &[u8; 20]) {
    entries.sort_by_key(|entry| xor_distance(&entry.node_id, target));
    let mut seen = HashSet::new();
    entries.retain(|entry| seen.insert(entry.node_id));
}

pub(super) fn xor_distance(left: &[u8; 20], right: &[u8; 20]) -> [u8; 20] {
    let mut distance = [0u8; 20];
    for (output, (left, right)) in distance.iter_mut().zip(left.iter().zip(right)) {
        *output = *left ^ *right;
    }
    distance
}

pub(super) async fn mark_node_good(
    addr: &SocketAddr,
    queried_node_id: &[u8; 20],
    message: &DhtMessage,
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
) -> Option<[u8; 20]> {
    let node_id = result_node_id(message)?;
    let mut routing_table = routing_table.write().await;
    if &node_id != queried_node_id {
        routing_table.remove(queried_node_id);
    }
    routing_table.mark_good(&node_id);
    routing_table.insert(DhtNode::new(node_id, *addr));
    Some(node_id)
}

pub(super) fn replace_lookup_node_id(
    entries: &mut [LookupEntry],
    addr: &SocketAddr,
    old_node_id: &[u8; 20],
    new_node_id: &[u8; 20],
) {
    if old_node_id == new_node_id {
        return;
    }
    if let Some(entry) = entries
        .iter_mut()
        .find(|entry| entry.addr == *addr && &entry.node_id == old_node_id)
    {
        entry.node_id = *new_node_id;
    }
}

pub(super) async fn mark_node_bad(
    node_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
) {
    let mut routing_table = routing_table.write().await;
    routing_table.mark_bad(node_id);
    routing_table.evict_bad_nodes();
}

pub(super) async fn add_node_to_table(
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    node: DhtNode,
) {
    routing_table.write().await.insert(node);
}
