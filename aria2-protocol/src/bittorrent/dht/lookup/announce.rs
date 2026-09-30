use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use tracing::debug;

use super::super::message::DhtMessageBuilder;
use super::super::routing_table::RoutingTable;
use super::super::socket::DhtSocket;
use super::super::task_impl::DhtTaskContext;
use super::super::tracker::{QueryType, TransactionTracker};
use super::K;
use tokio::sync::RwLock;

struct AnnounceContext<'a> {
    self_id: &'a [u8; 20],
    socket: &'a DhtSocket,
    tracker: &'a Arc<TransactionTracker>,
    query_timeout: Duration,
    routing_table: Option<&'a Arc<RwLock<RoutingTable>>>,
}

/// Send `announce_peer` to nodes that supplied tokens during `get_peers`.
///
/// Returns the number of requests accepted by the local UDP socket. This is a
/// send count, not a count of peer acknowledgements.
pub async fn announce_to_token_nodes(
    info_hash: &[u8; 20],
    self_id: &[u8; 20],
    port: u16,
    token_nodes: &[(SocketAddr, [u8; 20], Vec<u8>)],
    socket: &DhtSocket,
    tracker: &Arc<TransactionTracker>,
    query_timeout: Duration,
) -> usize {
    announce_to_token_nodes_inner(
        info_hash,
        port,
        token_nodes,
        AnnounceContext {
            self_id,
            socket,
            tracker,
            query_timeout,
            routing_table: None,
        },
    )
    .await
}

pub(in crate::bittorrent::dht) async fn announce_to_token_nodes_and_update_routing_table(
    info_hash: &[u8; 20],
    port: u16,
    token_nodes: &[(SocketAddr, [u8; 20], Vec<u8>)],
    context: &DhtTaskContext,
) {
    announce_to_token_nodes_inner(
        info_hash,
        port,
        token_nodes,
        AnnounceContext {
            self_id: &context.self_id,
            socket: &context.socket,
            tracker: &context.tracker,
            query_timeout: context.query_timeout,
            routing_table: Some(&context.routing_table),
        },
    )
    .await;
}

async fn announce_to_token_nodes_inner(
    info_hash: &[u8; 20],
    port: u16,
    token_nodes: &[(SocketAddr, [u8; 20], Vec<u8>)],
    context: AnnounceContext<'_>,
) -> usize {
    let mut sends = FuturesUnordered::new();

    for (addr, node_id, token) in token_nodes.iter().take(K) {
        let (transaction_id, response_wait) =
            context
                .tracker
                .allocate_wait(QueryType::AnnouncePeer, *addr, context.query_timeout);
        let message = DhtMessageBuilder::announce_peer_with_token(
            transaction_id,
            context.self_id,
            info_hash,
            port,
            token,
        );
        let encoded = message.encode();

        let socket = context.socket.clone();
        let addr = *addr;
        let node_id = *node_id;
        let routing_table = context.routing_table.cloned();
        sends.push(async move {
            if let Err(error) = socket.send_to(addr, &encoded).await {
                debug!(%addr, %error, "Failed to send announce_peer");
                return false;
            }
            if response_wait.wait().await.is_none()
                && let Some(routing_table) = routing_table
            {
                let mut routing_table = routing_table.write().await;
                routing_table.mark_bad(&node_id);
                routing_table.evict_bad_nodes();
            }
            true
        });
    }

    let mut sent_queries = 0;
    while let Some(sent) = sends.next().await {
        if sent {
            sent_queries += 1;
        }
    }
    sent_queries
}
