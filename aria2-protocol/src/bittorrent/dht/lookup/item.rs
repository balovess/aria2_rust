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
    ItemLookupResult, LookupPendingResponse, LookupRequest, MAX_ROUNDS, add_node_to_table,
    initialize_entries, insert_entry, mark_node_bad, mark_node_good, parse_stored_item,
    replace_lookup_node_id, send_batch_with_seq, sort_and_dedup,
};

/// Iteratively query the BEP 44 keyspace, returning the first valid value and
/// its response token.
pub async fn iterative_get_item(
    target: &[u8; 20],
    seq: Option<i64>,
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
) -> ItemLookupResult {
    iterative_get_item_with_token_collection(
        target,
        seq,
        self_id,
        routing_table,
        socket,
        tracker,
        query_timeout,
        false,
    )
    .await
}

/// Look up an item while collecting tokens from every contacted node for
/// redundant BEP 44 writes.
pub async fn iterative_get_item_for_publish(
    target: &[u8; 20],
    seq: Option<i64>,
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
) -> ItemLookupResult {
    iterative_get_item_with_token_collection(
        target,
        seq,
        self_id,
        routing_table,
        socket,
        tracker,
        query_timeout,
        true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn iterative_get_item_with_token_collection(
    target: &[u8; 20],
    seq: Option<i64>,
    self_id: &[u8; 20],
    routing_table: &Arc<tokio::sync::RwLock<RoutingTable>>,
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
    collect_all_tokens: bool,
) -> ItemLookupResult {
    let mut entries = initialize_entries(target, routing_table, self_id).await;
    let mut pending = FuturesUnordered::<LookupPendingResponse>::new();
    let mut tokens = Vec::new();
    let mut item = None;
    let mut rounds = 0;
    let request = LookupRequest {
        target,
        self_id,
        socket,
        tracker,
        query_type: QueryType::Get,
        query_timeout,
    };
    let mut nodes_contacted = send_batch_with_seq(&request, seq, &mut entries, &mut pending).await;
    while !pending.is_empty() && rounds < MAX_ROUNDS {
        rounds += 1;
        let Some(result) = pending.next().await else {
            break;
        };
        let queried_node_id = result.node_id;
        if let Some(response) = result.response {
            let from = response.from;
            let message = response.message;
            let responding_node_id =
                mark_node_good(&from, &queried_node_id, &message, routing_table).await;
            if let Some(responding_node_id) = responding_node_id {
                replace_lookup_node_id(&mut entries, &from, &queried_node_id, &responding_node_id);
            }
            if let Some(result) = message.r.as_ref() {
                if let Some(token) = result.dict_get(b"token").and_then(|value| value.as_bytes())
                    && let Some(node_id) = responding_node_id
                {
                    tokens.push((from, node_id, token.to_vec()));
                }
                if item.is_none()
                    && let Some(candidate) = parse_stored_item(target, result)
                    && candidate.verify_target()
                {
                    item = Some(candidate);
                    if !collect_all_tokens {
                        break;
                    }
                }
            }
            if collect_all_tokens && tokens.len() >= super::K {
                break;
            }
            for (addr, node_id) in extract_compact_nodes_from_response(&message) {
                add_node_to_table(routing_table, DhtNode::unverified(node_id, addr)).await;
                insert_entry(&mut entries, node_id, addr, target, self_id);
            }
            sort_and_dedup(&mut entries, target);
        } else {
            mark_node_bad(&queried_node_id, routing_table).await;
        }
        nodes_contacted += send_batch_with_seq(&request, seq, &mut entries, &mut pending).await;
    }
    ItemLookupResult {
        item,
        token_nodes: tokens,
        nodes_contacted,
    }
}
