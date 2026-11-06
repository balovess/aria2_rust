use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;

use super::super::compact::extract_compact_nodes_from_response;
use super::super::modern::SampleInfoHashesResponse;
use super::super::node::DhtNode;
use super::super::routing_table::RoutingTable;
use super::super::socket::DhtSocket;
use super::super::tracker::{QueryType, TransactionTracker};
use super::{
    LookupPendingResponse, LookupRequest, MAX_ROUNDS, SampleLookupResult, add_node_to_table,
    initialize_entries, insert_entry, mark_node_bad, mark_node_good, replace_lookup_node_id,
    send_batch_with_seq, sort_and_dedup,
};

/// Iteratively query nearby nodes for a BEP 51 sample. Responses are parsed
/// during traversal so malformed samples cannot influence discovery.
pub async fn iterative_sample_infohashes(
    target: &[u8; 20],
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
) -> SampleLookupResult {
    let mut entries = initialize_entries(target, routing_table, self_id).await;
    let mut pending = FuturesUnordered::<LookupPendingResponse>::new();
    let mut response = None;
    let mut rounds = 0;
    let request = LookupRequest {
        target,
        self_id,
        socket,
        tracker,
        query_type: QueryType::SampleInfohashes,
        query_timeout,
    };
    let mut nodes_contacted = send_batch_with_seq(&request, None, &mut entries, &mut pending).await;
    while !pending.is_empty() && rounds < MAX_ROUNDS {
        rounds += 1;
        let Some(result) = pending.next().await else {
            break;
        };
        let queried_node_id = result.node_id;
        if let Some(tracked) = result.response {
            let message = tracked.message;
            if let Some(responding_node_id) =
                mark_node_good(&tracked.from, &queried_node_id, &message, routing_table).await
            {
                replace_lookup_node_id(
                    &mut entries,
                    &tracked.from,
                    &queried_node_id,
                    &responding_node_id,
                );
            }
            if response.is_none() {
                response = message
                    .r
                    .as_ref()
                    .and_then(|value| SampleInfoHashesResponse::from_bencode(value).ok());
            }
            for (addr, node_id) in extract_compact_nodes_from_response(&message) {
                add_node_to_table(routing_table, DhtNode::unverified(node_id, addr)).await;
                insert_entry(&mut entries, node_id, addr, target, self_id);
            }
            sort_and_dedup(&mut entries, target);
        } else {
            mark_node_bad(&queried_node_id, routing_table).await;
        }
        nodes_contacted += send_batch_with_seq(&request, None, &mut entries, &mut pending).await;
    }
    SampleLookupResult {
        response,
        nodes_contacted,
    }
}
