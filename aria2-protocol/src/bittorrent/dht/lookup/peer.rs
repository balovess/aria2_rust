use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use super::super::compact::{
    extract_compact_nodes_from_response, extract_compact_peers_from_response,
};
use super::super::node::DhtNode;
use super::super::routing_table::RoutingTable;
use super::super::socket::DhtSocket;
use super::super::tracker::{QueryType, TransactionTracker};
use super::{
    K, LookupPendingResponse, LookupRequest, MAX_ROUNDS, PeerLookupResult, add_node_to_table,
    initialize_entries, insert_entry, mark_node_bad, mark_node_good, replace_lookup_node_id,
    send_batch_with_seq, sort_and_dedup, xor_distance,
};

/// Performs an iterative `get_peers` lookup for the given info hash.
///
/// Also collects peer addresses and tokens from responses. Tokens are needed
/// for a subsequent `announce_peer`.
pub async fn iterative_get_peers(
    info_hash: &[u8; 20],
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
) -> PeerLookupResult {
    let mut entries = initialize_entries(info_hash, routing_table, self_id).await;
    let mut pending = FuturesUnordered::<LookupPendingResponse>::new();
    let mut all_peers: Vec<SocketAddr> = Vec::new();
    let mut token_nodes: Vec<(SocketAddr, [u8; 20], Vec<u8>)> = Vec::new();
    let mut rounds = 0usize;
    let use_ipv6 = socket.local_addr().is_ipv6();
    let request = LookupRequest {
        target: info_hash,
        self_id,
        socket,
        tracker,
        query_type: QueryType::GetPeers,
        query_timeout,
    };

    let mut nodes_contacted = send_batch_with_seq(&request, None, &mut entries, &mut pending).await;

    while !pending.is_empty() && rounds < MAX_ROUNDS {
        rounds += 1;
        if let Some(result) = pending.next().await {
            let queried_node_id = result.node_id;
            if let Some(response) = result.response {
                let from = response.from;
                let message = response.message;
                let responding_node_id =
                    mark_node_good(&from, &queried_node_id, &message, routing_table).await;
                if let Some(responding_node_id) = responding_node_id {
                    replace_lookup_node_id(
                        &mut entries,
                        &from,
                        &queried_node_id,
                        &responding_node_id,
                    );
                }

                all_peers.extend(
                    extract_compact_peers_from_response(&message)
                        .into_iter()
                        .filter(|addr| addr.is_ipv6() == use_ipv6),
                );
                if let Some(token) = message
                    .r
                    .as_ref()
                    .and_then(|result| result.dict_get(b"token"))
                    .and_then(|value| value.as_bytes())
                    && let Some(node_id) = responding_node_id
                {
                    token_nodes.push((from, node_id, token.to_vec()));
                }

                for (addr, node_id) in extract_compact_nodes_from_response(&message) {
                    if addr.is_ipv6() != use_ipv6 {
                        continue;
                    }
                    add_node_to_table(routing_table, DhtNode::unverified(node_id, addr)).await;
                    insert_entry(&mut entries, node_id, addr, info_hash, self_id);
                }
                sort_and_dedup(&mut entries, info_hash);
            } else {
                mark_node_bad(&queried_node_id, routing_table).await;
            }
        }

        nodes_contacted += send_batch_with_seq(&request, None, &mut entries, &mut pending).await;
    }

    all_peers.sort_unstable();
    all_peers.dedup();
    token_nodes.sort_unstable_by_key(|(_, node_id, _)| xor_distance(node_id, info_hash));
    token_nodes.truncate(K);

    PeerLookupResult {
        peers: all_peers,
        token_nodes,
        nodes_contacted,
    }
}
