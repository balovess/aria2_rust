//! Peer-related DHT task implementations: PeerLookupTask and ReplaceNodeTask.
//!
//! These tasks correspond to the C++ peer-oriented task classes:
//!
//! - `PeerLookupTask`    ↔ C++ `DHTPeerLookupTask`
//! - `ReplaceNodeTask`   ↔ C++ `DHTReplaceNodeTask`
use std::net::SocketAddr;

use tracing::{debug, info, trace};

use super::lookup::{
    announce_to_token_nodes_and_update_routing_table, iterative_get_peers, result_node_id,
};
use super::message::DhtMessageBuilder;
use super::node::DhtNode;
use super::task::DhtTask;
use super::task_impl::DhtTaskContext;
use super::tracker::QueryType;

// ---------------------------------------------------------------------------
// PeerLookupTask
// ---------------------------------------------------------------------------

/// Perform an iterative `get_peers` lookup and optionally announce.
///
/// Equivalent to C++ `DHTPeerLookupTask`. Looks up peers for a given
/// info hash via the DHT network. If `announce_port` is set, also sends
/// `announce_peer` to the closest K nodes that provided tokens.
#[derive(Debug)]
pub struct PeerLookupTask {
    ctx: DhtTaskContext,
    /// Info hash to look up peers for.
    info_hash: [u8; 20],
    /// If non-zero, announce this port after finding peers.
    announce_port: u16,
    /// Channel to deliver the discovered peers.
    result_tx: Option<tokio::sync::oneshot::Sender<PeerLookupResult>>,
}

/// Result of a peer lookup task.
#[derive(Debug, Clone)]
pub struct PeerLookupResult {
    /// Discovered peer addresses.
    pub peers: Vec<SocketAddr>,
    /// Number of queries accepted by the local UDP socket (whether or not nodes reply).
    pub nodes_contacted: usize,
}

impl PeerLookupTask {
    /// Create a new peer lookup task.
    ///
    /// If `result_tx` is provided, the result will be sent when the task
    /// completes. If `announce_port` is non-zero, `announce_peer` messages
    /// are sent after the lookup.
    pub fn new(
        ctx: DhtTaskContext,
        info_hash: [u8; 20],
        announce_port: u16,
        result_tx: Option<tokio::sync::oneshot::Sender<PeerLookupResult>>,
    ) -> Self {
        Self {
            ctx,
            info_hash,
            announce_port,
            result_tx,
        }
    }
}

#[async_trait::async_trait]
impl DhtTask for PeerLookupTask {
    async fn run(self: Box<Self>) {
        let result = iterative_get_peers(
            &self.info_hash,
            &self.ctx.self_id,
            &self.ctx.routing_table,
            &self.ctx.socket,
            &self.ctx.tracker,
            self.ctx.query_timeout,
        )
        .await;

        // Announce to token nodes if requested.
        if self.announce_port > 0 && !result.token_nodes.is_empty() {
            announce_to_token_nodes_and_update_routing_table(
                &self.info_hash,
                self.announce_port,
                &result.token_nodes,
                &self.ctx,
            )
            .await;
        }

        debug!(
            info_hash = %hex::encode(self.info_hash),
            peers = result.peers.len(),
            contacted = result.nodes_contacted,
            "PeerLookupTask completed"
        );

        // Deliver result if a channel was provided.
        if let Some(tx) = self.result_tx {
            let _ = tx.send(PeerLookupResult {
                peers: result.peers,
                nodes_contacted: result.nodes_contacted,
            });
        }
    }

    fn name(&self) -> &'static str {
        "PeerLookupTask"
    }
}

// ---------------------------------------------------------------------------
// ReplaceNodeTask
// ---------------------------------------------------------------------------

/// Verify questionable nodes and replace them with cached candidates.
///
/// Equivalent to C++ `DHTReplaceNodeTask`. Pings the LRU questionable
/// node in the bucket. If it doesn't respond after `MAX_RETRY` attempts,
/// the questionable node is replaced with the new node.
#[derive(Debug)]
pub(super) struct ReplaceNodeTask {
    ctx: DhtTaskContext,
    /// Node identifying the bucket and replacement target.
    questionable_node_id: [u8; 20],
    /// New node to potentially insert.
    new_node: DhtNode,
}

impl ReplaceNodeTask {
    pub(super) fn new(
        ctx: DhtTaskContext,
        questionable_node_id: [u8; 20],
        new_node: DhtNode,
    ) -> Self {
        Self {
            ctx,
            questionable_node_id,
            new_node,
        }
    }
}

#[async_trait::async_trait]
impl DhtTask for ReplaceNodeTask {
    async fn run(self: Box<Self>) {
        // Find the bucket and extract the questionable node info.
        let (q_id, q_addr) = {
            let rt = self.ctx.routing_table.read().await;
            let bucket = rt.get_bucket_for(&self.questionable_node_id);

            let Some(node) = bucket
                .nodes()
                .iter()
                .find(|node| node.id == self.questionable_node_id && node.is_questionable())
            else {
                trace!("ReplaceNodeTask: no questionable node available");
                return;
            };
            (node.id, node.addr)
        };

        // Send ping to the questionable node with retry.
        for attempt in 0..2 {
            let (transaction_id, response_wait) =
                self.ctx
                    .tracker
                    .allocate_wait(QueryType::Ping, q_addr, self.ctx.query_timeout);
            let msg = DhtMessageBuilder::ping(transaction_id, &self.ctx.self_id);
            let encoded = msg.encode();

            if let Err(e) = self.ctx.socket.send_to(q_addr, &encoded).await {
                debug!(
                    "ReplaceNodeTask: send error to {}: {}",
                    hex::encode(q_id),
                    e
                );
                return;
            }

            if let Some(response) = response_wait.wait().await
                && response.from == q_addr
                && response.message.is_response()
                && let Some(responding_node_id) = result_node_id(&response.message)
                && responding_node_id != self.ctx.self_id
            {
                info!(
                    "ReplaceNodeTask: ping reply received from {}",
                    hex::encode(responding_node_id)
                );
                let mut rt = self.ctx.routing_table.write().await;
                if !rt.replace_node_identity(&q_id, DhtNode::new(responding_node_id, q_addr)) {
                    trace!(
                        old_node = %hex::encode(q_id),
                        responding_node = %hex::encode(responding_node_id),
                        "ReplaceNodeTask: queried node changed identity before the response was applied"
                    );
                }
                return;
            }
            let mut rt = self.ctx.routing_table.write().await;
            rt.mark_bad(&q_id);
            rt.evict_bad_nodes();
            if attempt < 1 {
                debug!(
                    "ReplaceNodeTask: ping timeout from {}, retrying",
                    hex::encode(q_id)
                );
            }
        }

        // All retries exhausted — replace the questionable node.
        info!(
            "ReplaceNodeTask: replacing {} with {}",
            hex::encode(q_id),
            self.new_node.id_hex(),
        );

        let mut rt = self.ctx.routing_table.write().await;
        if !rt.replace_node(&q_id, self.new_node.clone()) {
            trace!(
                "ReplaceNodeTask: replacement candidate {} is no longer available",
                self.new_node.id_hex()
            );
        }
    }

    fn name(&self) -> &'static str {
        "ReplaceNodeTask"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::super::routing_table::RoutingTable;
    use super::super::socket::DhtSocket;
    use super::super::tracker::TransactionTracker;
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::RwLock;

    #[test]
    fn test_peer_lookup_result() {
        let result = PeerLookupResult {
            peers: vec!["127.0.0.1:6881".parse().unwrap()],
            nodes_contacted: 5,
        };
        assert_eq!(result.peers.len(), 1);
        assert_eq!(result.nodes_contacted, 5);
    }

    #[tokio::test]
    async fn test_peer_lookup_does_not_reacquire_shared_table() {
        let ctx = DhtTaskContext::new(
            [0u8; 20],
            Arc::new(RwLock::new(RoutingTable::new([0u8; 20]))),
            DhtSocket::bind(0).await.expect("test socket should bind"),
            Arc::new(TransactionTracker::new()),
            Duration::from_millis(20),
        );

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            Box::new(PeerLookupTask::new(ctx, [1u8; 20], 0, None)).run(),
        )
        .await;

        assert!(result.is_ok(), "peer lookup task should not deadlock");
    }
}
