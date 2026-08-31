//! Peer-related DHT task implementations: PeerLookupTask, ReplaceNodeTask,
//! PeerAnnounceTask, and DhtTaskFactory.
//!
//! These tasks correspond to the C++ peer-oriented task classes:
//!
//! - `PeerLookupTask`    ↔ C++ `DHTPeerLookupTask`
//! - `ReplaceNodeTask`   ↔ C++ `DHTReplaceNodeTask`
//! - `PeerAnnounceTask`  ↔ C++ `DHTPeerAnnounceTask` (stub in C++)
//!
//! Also contains `DhtTaskFactory`, equivalent to C++ `DHTTaskFactoryImpl`,
//! which bundles the common context so that task creation is a single
//! method call.

use std::net::SocketAddr;
use std::time::Duration;

use tracing::{debug, info, trace, warn};

use super::lookup::{announce_to_token_nodes, iterative_get_peers};
use super::message::DhtMessageBuilder;
use super::node::DhtNode;
use super::task::BoxedDhtTask;
use super::task::DhtTask;
use super::task_impl::{
    BootstrapRefreshTask, BucketRefreshTask, DhtTaskContext, NodeLookupTask, PingTask,
};
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
    /// Number of nodes contacted.
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
            announce_to_token_nodes(
                &self.info_hash,
                &self.ctx.self_id,
                self.announce_port,
                &result.token_nodes,
                &self.ctx.socket,
                &self.ctx.tracker,
                self.ctx.query_timeout,
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
pub struct ReplaceNodeTask {
    ctx: DhtTaskContext,
    /// Optional exact node identifying the bucket and replacement target.
    questionable_node_id: Option<[u8; 20]>,
    /// Optional legacy bucket-prefix selector.
    bucket_prefix_length: Option<usize>,
    /// New node to potentially insert.
    new_node: DhtNode,
}

impl ReplaceNodeTask {
    /// Create a new replace node task.
    ///
    /// Create a task using the legacy bucket-prefix selector.
    pub fn new(ctx: DhtTaskContext, bucket_prefix_length: usize, new_node: DhtNode) -> Self {
        Self {
            ctx,
            questionable_node_id: None,
            bucket_prefix_length: Some(bucket_prefix_length),
            new_node,
        }
    }

    /// Create a task for an exact questionable node.
    pub fn new_for_node(
        ctx: DhtTaskContext,
        questionable_node_id: [u8; 20],
        new_node: DhtNode,
    ) -> Self {
        Self {
            ctx,
            questionable_node_id: Some(questionable_node_id),
            bucket_prefix_length: None,
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
            let bucket = if let Some(node_id) = self.questionable_node_id {
                let Some(bucket) = rt.get_bucket_for(&node_id) else {
                    trace!(
                        "ReplaceNodeTask: bucket not found for node {}",
                        hex::encode(node_id)
                    );
                    return;
                };
                bucket
            } else {
                let Some(prefix_length) = self.bucket_prefix_length else {
                    return;
                };
                let Some(bucket) = rt
                    .get_all_buckets()
                    .into_iter()
                    .find(|bucket| bucket.prefix_length() == prefix_length)
                else {
                    trace!("ReplaceNodeTask: bucket not found prefix={}", prefix_length);
                    return;
                };
                bucket
            };

            let Some(node) = bucket.nodes().iter().find(|node| {
                self.questionable_node_id
                    .map(|id| node.id == id)
                    .unwrap_or(true)
                    && node.is_questionable()
            }) else {
                trace!("ReplaceNodeTask: no questionable node available");
                return;
            };
            (node.id, node.addr)
        };

        // Send ping to the questionable node with retry.
        for attempt in 0..2 {
            let (transaction_id, response_wait) = self.ctx.tracker.allocate_wait(
                QueryType::Ping,
                q_addr,
                Some(q_id),
                None,
                self.ctx.query_timeout,
            );
            let msg = DhtMessageBuilder::ping(transaction_id, &self.ctx.self_id);
            let encoded = match msg.encode() {
                Ok(e) => e,
                Err(e) => {
                    warn!("ReplaceNodeTask: encode error: {}", e);
                    return;
                }
            };

            if let Err(e) = self.ctx.socket.send_to(q_addr, &encoded).await {
                debug!(
                    "ReplaceNodeTask: send error to {}: {}",
                    hex::encode(q_id),
                    e
                );
                return;
            }

            if let Some(response) = response_wait.wait(self.ctx.query_timeout).await
                && response.from == q_addr
                && (response.message.is_response() || response.message.is_error())
            {
                info!(
                    "ReplaceNodeTask: ping reply received from {} — node is alive",
                    hex::encode(q_id)
                );
                let mut rt = self.ctx.routing_table.write().await;
                rt.mark_good(&q_id);
                return;
            }
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
        rt.mark_bad(&q_id);
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
// PeerAnnounceTask
// ---------------------------------------------------------------------------

/// Announce that we are serving a torrent identified by `info_hash`.
///
/// Equivalent to C++ `DHTPeerAnnounceTask` (which is a stub in the
/// original C++ code). Performs a `get_peers` lookup to obtain tokens,
/// then sends `announce_peer` to the closest K nodes.
#[derive(Debug)]
pub struct PeerAnnounceTask {
    ctx: DhtTaskContext,
    /// Info hash to announce for.
    info_hash: [u8; 20],
    /// Port we are listening on for the torrent.
    port: u16,
}

impl PeerAnnounceTask {
    /// Create a new peer announce task.
    pub fn new(ctx: DhtTaskContext, info_hash: [u8; 20], port: u16) -> Self {
        Self {
            ctx,
            info_hash,
            port,
        }
    }
}

#[async_trait::async_trait]
impl DhtTask for PeerAnnounceTask {
    async fn run(self: Box<Self>) {
        debug!(
            info_hash = %hex::encode(self.info_hash),
            port = self.port,
            "PeerAnnounceTask: starting get_peers lookup"
        );

        let result = iterative_get_peers(
            &self.info_hash,
            &self.ctx.self_id,
            &self.ctx.routing_table,
            &self.ctx.socket,
            &self.ctx.tracker,
            self.ctx.query_timeout,
        )
        .await;

        // Announce to token nodes.
        if result.token_nodes.is_empty() {
            debug!(
                info_hash = %hex::encode(self.info_hash),
                "PeerAnnounceTask: no token nodes found, cannot announce"
            );
            return;
        }

        let announced = announce_to_token_nodes(
            &self.info_hash,
            &self.ctx.self_id,
            self.port,
            &result.token_nodes,
            &self.ctx.socket,
            &self.ctx.tracker,
            self.ctx.query_timeout,
        )
        .await;

        info!(
            info_hash = %hex::encode(self.info_hash),
            announced,
            "PeerAnnounceTask completed"
        );
    }

    fn name(&self) -> &'static str {
        "PeerAnnounceTask"
    }
}

// ---------------------------------------------------------------------------
// TaskFactory — convenience for creating tasks with a shared context
// ---------------------------------------------------------------------------

/// Factory for creating DHT tasks with a shared context.
///
/// Equivalent to C++ `DHTTaskFactoryImpl`. Bundles the common resources
/// (routing table, socket, tracker, self_id) so that task creation is
/// a single method call without repeating all the plumbing.
#[derive(Clone)]
pub struct DhtTaskFactory {
    ctx: DhtTaskContext,
}

impl DhtTaskFactory {
    /// Create a new task factory with the given context.
    pub fn new(ctx: DhtTaskContext) -> Self {
        Self { ctx }
    }

    /// Create a ping task for the given remote node.
    pub fn create_ping_task(&self, remote_node: DhtNode, max_retry: u32) -> BoxedDhtTask {
        Box::new(PingTask::new(self.ctx.clone(), remote_node, max_retry))
    }

    /// Create a ping task that returns the node ID from a successful reply.
    pub fn create_ping_task_with_result(
        &self,
        remote_node: DhtNode,
        max_retry: u32,
        result_tx: tokio::sync::oneshot::Sender<Option<DhtNode>>,
    ) -> BoxedDhtTask {
        Box::new(PingTask::with_result(
            self.ctx.clone(),
            remote_node,
            max_retry,
            result_tx,
        ))
    }

    /// Create a bucket refresh task.
    pub fn create_bucket_refresh_task(&self, force_refresh: bool) -> BoxedDhtTask {
        Box::new(BucketRefreshTask::new(self.ctx.clone(), force_refresh))
    }

    /// Create the bounded first refresh used by bootstrap.
    pub fn create_bootstrap_refresh_task(&self, timeout: Duration) -> BoxedDhtTask {
        Box::new(BootstrapRefreshTask::new(self.ctx.clone(), timeout))
    }

    /// Create a node lookup task for the given target ID.
    pub fn create_node_lookup_task(&self, target_id: [u8; 20]) -> BoxedDhtTask {
        Box::new(NodeLookupTask::new(self.ctx.clone(), target_id))
    }

    /// Create a peer lookup task for the given info hash.
    pub fn create_peer_lookup_task(
        &self,
        info_hash: [u8; 20],
        announce_port: u16,
        result_tx: Option<tokio::sync::oneshot::Sender<PeerLookupResult>>,
    ) -> BoxedDhtTask {
        Box::new(PeerLookupTask::new(
            self.ctx.clone(),
            info_hash,
            announce_port,
            result_tx,
        ))
    }

    /// Create a replace node task.
    pub fn create_replace_node_task(
        &self,
        bucket_prefix_length: usize,
        new_node: DhtNode,
    ) -> BoxedDhtTask {
        Box::new(ReplaceNodeTask::new(
            self.ctx.clone(),
            bucket_prefix_length,
            new_node,
        ))
    }

    /// Create a replacement task for an exact questionable node.
    pub fn create_replace_node_for_node(
        &self,
        questionable_node_id: [u8; 20],
        new_node: DhtNode,
    ) -> BoxedDhtTask {
        Box::new(ReplaceNodeTask::new_for_node(
            self.ctx.clone(),
            questionable_node_id,
            new_node,
        ))
    }

    /// Create a peer announce task.
    pub fn create_peer_announce_task(&self, info_hash: [u8; 20], port: u16) -> BoxedDhtTask {
        Box::new(PeerAnnounceTask::new(self.ctx.clone(), info_hash, port))
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
    fn test_task_factory_creation() {
        let ctx = DhtTaskContext {
            self_id: [0u8; 20],
            routing_table: Arc::new(RwLock::new(RoutingTable::new([0u8; 20]))),
            socket: DhtSocket::new_test(),
            tracker: Arc::new(TransactionTracker::new()),
            query_timeout: Duration::from_secs(10),
        };
        let factory = DhtTaskFactory::new(ctx);

        // Verify we can create tasks without panicking.
        let _ping = factory.create_ping_task(
            DhtNode::new([1u8; 20], "127.0.0.1:6881".parse().unwrap()),
            0,
        );
        let _refresh = factory.create_bucket_refresh_task(false);
        let _lookup = factory.create_node_lookup_task([2u8; 20]);
        let _announce = factory.create_peer_announce_task([3u8; 20], 6881);
    }

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
        let ctx = DhtTaskContext {
            self_id: [0u8; 20],
            routing_table: Arc::new(RwLock::new(RoutingTable::new([0u8; 20]))),
            socket: DhtSocket::bind(0).await.expect("test socket should bind"),
            tracker: Arc::new(TransactionTracker::new()),
            query_timeout: Duration::from_millis(20),
        };

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            Box::new(PeerLookupTask::new(ctx, [1u8; 20], 0, None)).run(),
        )
        .await;

        assert!(result.is_ok(), "peer lookup task should not deadlock");
    }
}
