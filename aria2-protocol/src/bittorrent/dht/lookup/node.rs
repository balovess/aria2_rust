use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use super::super::compact::extract_compact_nodes_from_response;
use super::super::node::DhtNode;
use super::super::routing_table::RoutingTable;
use super::super::socket::DhtSocket;
use super::super::tracker::{QueryType, TransactionTracker};
use super::{
    K, LookupPendingResponse, LookupRequest, MAX_ROUNDS, NodeLookupResult, add_node_to_table,
    initialize_entries, insert_entry, mark_node_bad, mark_node_good, replace_lookup_node_id,
    send_batch_with_seq, sort_and_dedup,
};

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
    let mut entries = initialize_entries(target, routing_table, self_id).await;
    let mut pending = FuturesUnordered::<LookupPendingResponse>::new();
    let mut rounds = 0usize;
    let use_ipv6 = socket.local_addr().is_ipv6();
    let request = LookupRequest {
        target,
        self_id,
        socket,
        tracker,
        query_type: QueryType::FindNode,
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
                if let Some(responding_node_id) =
                    mark_node_good(&from, &queried_node_id, &message, routing_table).await
                {
                    replace_lookup_node_id(
                        &mut entries,
                        &from,
                        &queried_node_id,
                        &responding_node_id,
                    );
                }

                for (addr, node_id) in extract_compact_nodes_from_response(&message) {
                    if addr.is_ipv6() != use_ipv6 {
                        continue;
                    }
                    add_node_to_table(routing_table, DhtNode::unverified(node_id, addr)).await;
                    insert_entry(&mut entries, node_id, addr, target, self_id);
                }
                sort_and_dedup(&mut entries, target);
            } else {
                mark_node_bad(&queried_node_id, routing_table).await;
            }
        }

        nodes_contacted += send_batch_with_seq(&request, None, &mut entries, &mut pending).await;
    }

    let closest_nodes = entries
        .iter()
        .take(K)
        .map(|entry| DhtNode::unverified(entry.node_id, entry.addr))
        .collect();

    NodeLookupResult {
        closest_nodes,
        nodes_contacted,
    }
}
