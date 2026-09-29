//! Core DHT task implementations: PingTask, BucketRefreshTask, NodeLookupTask.
//!
//! See `task_peer.rs` for peer-related tasks (PeerLookupTask,
//! ReplaceNodeTask, PeerAnnounceTask) and the DhtTaskFactory.

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::{debug, info, trace, warn};

use super::lookup::iterative_find_node;
use super::message::DhtMessageBuilder;
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::task::{DEFAULT_NUM_CONCURRENT, DhtTask};
use super::tracker::{QueryType, TransactionTracker};

const BUCKET_REFRESH_CONCURRENCY: usize = 3;

// ---------------------------------------------------------------------------
// Shared context for tasks that need engine resources
// ---------------------------------------------------------------------------

/// Shared context providing access to DHT engine resources.
///
/// In the C++ design, each `DHTAbstractTask` holds raw pointers to the
/// routing table, message dispatcher, message factory, and task queue.
/// In Rust, we bundle these behind `Arc<RwLock<>>` and `Arc<Mutex>` for
/// safe shared access across async tasks.
#[derive(Clone)]
pub struct DhtTaskContext {
    /// Local node ID.
    pub(crate) self_id: [u8; 20],
    /// Routing table (shared with the DHT engine).
    pub(crate) routing_table: Arc<RwLock<RoutingTable>>,
    /// UDP socket for sending messages.
    pub(crate) socket: DhtSocket,
    /// Transaction tracker for matching queries to responses.
    pub(crate) tracker: Arc<TransactionTracker>,
    /// Per-query timeout (C++ `DHT_MESSAGE_TIMEOUT = 10s`).
    pub(crate) query_timeout: Duration,
}

impl DhtTaskContext {
    pub fn new(
        self_id: [u8; 20],
        routing_table: Arc<RwLock<RoutingTable>>,
        socket: DhtSocket,
        tracker: Arc<TransactionTracker>,
        query_timeout: Duration,
    ) -> Self {
        Self {
            self_id,
            routing_table,
            socket,
            tracker,
            query_timeout,
        }
    }

    pub fn self_id(&self) -> &[u8; 20] {
        &self.self_id
    }

    pub fn routing_table(&self) -> &Arc<RwLock<RoutingTable>> {
        &self.routing_table
    }

    pub fn socket(&self) -> &DhtSocket {
        &self.socket
    }

    pub fn tracker(&self) -> &Arc<TransactionTracker> {
        &self.tracker
    }

    pub fn query_timeout(&self) -> Duration {
        self.query_timeout
    }
}

impl std::fmt::Debug for DhtTaskContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtTaskContext")
            .field("self_id", &hex::encode(self.self_id))
            .field("query_timeout", &self.query_timeout)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// PingTask
// ---------------------------------------------------------------------------

/// Send a ping to a remote node and wait for a response.
///
/// Equivalent to C++ `DHTPingTask`. If the node responds, it is marked
/// good in the routing table. If it times out after `max_retry` attempts,
/// it is marked as failed.
#[derive(Debug)]
pub struct PingTask {
    ctx: DhtTaskContext,
    /// Remote node to ping.
    remote_node: DhtNode,
    /// Maximum retry count on timeout (C++ default: 0).
    max_retry: u32,
    /// Per-attempt timeout.
    timeout: Duration,
    /// Optional result channel used when adding an unknown bootstrap node.
    result_tx: Option<tokio::sync::oneshot::Sender<Option<DhtNode>>>,
}

impl PingTask {
    /// Create a new ping task.
    pub fn new(ctx: DhtTaskContext, remote_node: DhtNode, max_retry: u32) -> Self {
        Self {
            timeout: ctx.query_timeout,
            ctx,
            remote_node,
            max_retry,
            result_tx: None,
        }
    }

    /// Create a ping task that returns the node ID from a successful reply.
    pub fn with_result(
        ctx: DhtTaskContext,
        remote_node: DhtNode,
        max_retry: u32,
        result_tx: tokio::sync::oneshot::Sender<Option<DhtNode>>,
    ) -> Self {
        Self {
            timeout: ctx.query_timeout,
            ctx,
            remote_node,
            max_retry,
            result_tx: Some(result_tx),
        }
    }
}

#[async_trait::async_trait]
impl DhtTask for PingTask {
    async fn run(self: Box<Self>) {
        let addr = self.remote_node.addr;
        let node_id = self.remote_node.id;
        let mut result_tx = self.result_tx;

        for attempt in 0..=self.max_retry {
            if attempt > 0 {
                debug!(
                    attempt,
                    max_retry = self.max_retry,
                    "PingTask retrying node {}",
                    hex::encode(node_id),
                );
            }

            let (transaction_id, response_wait) = self.ctx.tracker.allocate_wait(
                QueryType::Ping,
                addr,
                Some(node_id),
                None,
                self.timeout,
            );
            let msg = DhtMessageBuilder::ping(transaction_id, &self.ctx.self_id);
            let encoded = match msg.encode() {
                Ok(e) => e,
                Err(e) => {
                    warn!("PingTask: encode error for {}: {}", hex::encode(node_id), e);
                    if let Some(tx) = result_tx.take() {
                        let _ = tx.send(None);
                    }
                    return;
                }
            };

            if let Err(e) = self.ctx.socket.send_to(addr, &encoded).await {
                debug!("PingTask: send error to {}: {}", hex::encode(node_id), e);
                if let Some(tx) = result_tx.take() {
                    let _ = tx.send(None);
                }
                return;
            }

            if let Some(response) = response_wait.wait(self.timeout).await
                && response.from == addr
                && response.message.is_response()
            {
                let response_node = response
                    .message
                    .r
                    .as_ref()
                    .and_then(|result| result.dict_get(b"id"))
                    .and_then(|id| id.as_bytes())
                    .filter(|id| id.len() == 20)
                    .map(|id| {
                        let mut response_id = [0u8; 20];
                        response_id.copy_from_slice(id);
                        DhtNode::new(response_id, addr)
                    });

                if let Some(response_node) = response_node {
                    trace!(
                        "PingTask: node {} responded",
                        hex::encode(response_node.id())
                    );
                    let mut rt = self.ctx.routing_table.write().await;
                    if node_id != [0u8; 20] && node_id != *response_node.id() {
                        rt.remove(&node_id);
                    } else if node_id != [0u8; 20] {
                        rt.mark_good(&node_id);
                    }
                    rt.insert(response_node.clone());
                    if let Some(tx) = result_tx.take() {
                        let _ = tx.send(Some(response_node));
                    }
                    return;
                }
            }
        }

        // All attempts exhausted — mark as failed.
        debug!(
            "PingTask: node {} timed out after {} attempts",
            hex::encode(node_id),
            self.max_retry + 1
        );
        let mut rt = self.ctx.routing_table.write().await;
        rt.mark_bad(&node_id);
        if let Some(tx) = result_tx {
            let _ = tx.send(None);
        }
    }

    fn name(&self) -> &'static str {
        "PingTask"
    }
}

// ---------------------------------------------------------------------------
// BucketRefreshTask
// ---------------------------------------------------------------------------

/// Refresh stale k-buckets by performing `find_node` lookups.
///
/// Equivalent to C++ `DHTBucketRefreshTask`. Iterates over all buckets
/// and issues a `find_node` lookup for a random ID in each bucket that
/// needs refreshing (fewer than K nodes or not updated in 15 minutes).
///
/// When `force_refresh` is true, all buckets are refreshed regardless of
/// their staleness. This is used during bootstrap.
#[derive(Debug)]
pub struct BucketRefreshTask {
    ctx: DhtTaskContext,
    /// If true, refresh all buckets regardless of staleness.
    force_refresh: bool,
}

impl BucketRefreshTask {
    /// Create a new bucket refresh task.
    pub fn new(ctx: DhtTaskContext, force_refresh: bool) -> Self {
        Self { ctx, force_refresh }
    }
}

#[async_trait::async_trait]
impl DhtTask for BucketRefreshTask {
    async fn run(self: Box<Self>) {
        let targets = {
            let rt = self.ctx.routing_table.read().await;
            if self.force_refresh {
                rt.get_all_buckets()
                    .iter()
                    .map(|b| b.get_random_node_id())
                    .collect::<Vec<_>>()
            } else {
                rt.refresh_buckets()
            }
        };

        if targets.is_empty() {
            trace!("BucketRefreshTask: no buckets need refresh");
            return;
        }

        info!(count = targets.len(), "Dispatching bucket refresh lookups");

        let ctx = &self.ctx;
        futures::stream::iter(targets)
            .map(|target| async move {
                let result = iterative_find_node(
                    &target,
                    &ctx.self_id,
                    &ctx.routing_table,
                    &ctx.socket,
                    &ctx.tracker,
                    ctx.query_timeout,
                )
                .await;
                (target, result)
            })
            .buffer_unordered(BUCKET_REFRESH_CONCURRENCY)
            .for_each(|(target, result)| async move {
                trace!(
                    target = %hex::encode(target),
                    nodes_contacted = result.nodes_contacted,
                    "Bucket refresh lookup completed"
                );
            })
            .await;
    }

    fn name(&self) -> &'static str {
        "BucketRefreshTask"
    }
}

/// Run the initial forced refresh with an explicit bootstrap deadline.
#[derive(Debug)]
pub struct BootstrapRefreshTask {
    ctx: DhtTaskContext,
    timeout: Duration,
    bootstrap_nodes: Vec<DhtNode>,
}

impl BootstrapRefreshTask {
    pub fn new(ctx: DhtTaskContext, timeout: Duration, bootstrap_nodes: Vec<DhtNode>) -> Self {
        Self {
            ctx,
            timeout,
            bootstrap_nodes,
        }
    }
}

#[async_trait::async_trait]
impl DhtTask for BootstrapRefreshTask {
    async fn run(self: Box<Self>) {
        const BOOTSTRAP_PING_MAX_RETRIES: u32 = 9;

        let ctx = self.ctx;
        let bootstrap_nodes = self.bootstrap_nodes;
        let refresh = async move {
            futures::stream::iter(bootstrap_nodes.into_iter().map(|node| {
                let ctx = ctx.clone();
                async move {
                    Box::new(PingTask::new(ctx, node, BOOTSTRAP_PING_MAX_RETRIES))
                        .run()
                        .await;
                }
            }))
            .buffer_unordered(DEFAULT_NUM_CONCURRENT)
            .for_each(|_| async {})
            .await;

            Box::new(BucketRefreshTask::new(ctx, true)).run().await;
        };

        if tokio::time::timeout(self.timeout, refresh).await.is_err() {
            warn!(timeout = ?self.timeout, "DHT bootstrap refresh timed out");
        }
    }

    fn name(&self) -> &'static str {
        "BootstrapRefreshTask"
    }
}

// ---------------------------------------------------------------------------
// NodeLookupTask
// ---------------------------------------------------------------------------

/// Perform an iterative `find_node` lookup for a target node ID.
///
/// Equivalent to C++ `DHTNodeLookupTask`. Used for bucket refresh
/// and general routing table population.
#[derive(Debug)]
pub struct NodeLookupTask {
    ctx: DhtTaskContext,
    /// Target node ID to find.
    target_id: [u8; 20],
}

impl NodeLookupTask {
    /// Create a new node lookup task.
    pub fn new(ctx: DhtTaskContext, target_id: [u8; 20]) -> Self {
        Self { ctx, target_id }
    }
}

#[async_trait::async_trait]
impl DhtTask for NodeLookupTask {
    async fn run(self: Box<Self>) {
        let result = iterative_find_node(
            &self.target_id,
            &self.ctx.self_id,
            &self.ctx.routing_table,
            &self.ctx.socket,
            &self.ctx.tracker,
            self.ctx.query_timeout,
        )
        .await;

        debug!(
            target = %hex::encode(self.target_id),
            closest = result.closest_nodes.len(),
            contacted = result.nodes_contacted,
            "NodeLookupTask completed"
        );
    }

    fn name(&self) -> &'static str {
        "NodeLookupTask"
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_context_creation() {
        let ctx = DhtTaskContext::new(
            [0u8; 20],
            Arc::new(RwLock::new(RoutingTable::new([0u8; 20]))),
            DhtSocket::new_test(),
            Arc::new(TransactionTracker::new()),
            Duration::from_secs(10),
        );
        assert_eq!(ctx.self_id(), &[0u8; 20]);
        assert_eq!(ctx.query_timeout(), Duration::from_secs(10));
    }

    #[tokio::test]
    async fn test_node_lookup_does_not_reacquire_shared_table() {
        let routing_table = Arc::new(RwLock::new(RoutingTable::new([0u8; 20])));
        let ctx = DhtTaskContext::new(
            [0u8; 20],
            routing_table,
            DhtSocket::bind(0).await.expect("test socket should bind"),
            Arc::new(TransactionTracker::new()),
            Duration::from_millis(20),
        );

        let result = tokio::time::timeout(
            Duration::from_millis(200),
            Box::new(NodeLookupTask::new(ctx, [1u8; 20])).run(),
        )
        .await;

        assert!(result.is_ok(), "lookup task should not deadlock");
    }
}
