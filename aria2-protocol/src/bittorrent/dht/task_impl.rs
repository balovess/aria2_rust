//! Core DHT task implementations: PingTask, BucketRefreshTask, NodeLookupTask.
//!
//! See `task_peer.rs` for peer-related tasks (PeerLookupTask,
//! ReplaceNodeTask, PeerAnnounceTask) and the DhtTaskFactory.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tracing::{debug, info, trace, warn};

use super::lookup::iterative_find_node;
use super::message::DhtMessageBuilder;
use super::node::DhtNode;
use super::routing_table::RoutingTable;
use super::socket::DhtSocket;
use super::task::DhtTask;
use super::tracker::TransactionTracker;

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
    pub self_id: [u8; 20],
    /// Routing table (shared with the DHT engine).
    pub routing_table: Arc<RwLock<RoutingTable>>,
    /// UDP socket for sending messages.
    pub socket: DhtSocket,
    /// Transaction tracker for matching queries to responses.
    pub tracker: Arc<TransactionTracker>,
    /// Per-query timeout (C++ `DHT_MESSAGE_TIMEOUT = 10s`).
    pub query_timeout: Duration,
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
}

impl PingTask {
    /// Create a new ping task.
    pub fn new(ctx: DhtTaskContext, remote_node: DhtNode, max_retry: u32) -> Self {
        Self {
            timeout: ctx.query_timeout,
            ctx,
            remote_node,
            max_retry,
        }
    }
}

#[async_trait::async_trait]
impl DhtTask for PingTask {
    async fn run(self: Box<Self>) {
        let addr = self.remote_node.addr;
        let node_id = self.remote_node.id;

        for attempt in 0..=self.max_retry {
            if attempt > 0 {
                debug!(
                    attempt,
                    max_retry = self.max_retry,
                    "PingTask retrying node {}",
                    hex::encode(node_id),
                );
            }

            let msg = DhtMessageBuilder::ping(0, &self.ctx.self_id);
            let encoded = match msg.encode() {
                Ok(e) => e,
                Err(e) => {
                    warn!("PingTask: encode error for {}: {}", hex::encode(node_id), e);
                    return;
                }
            };

            if let Err(e) = self.ctx.socket.send_to(addr, &encoded).await {
                debug!("PingTask: send error to {}: {}", hex::encode(node_id), e);
                return;
            }

            // Wait for a response with timeout.
            let mut buf = [0u8; 4096];
            match self
                .ctx
                .socket
                .recv_with_timeout(&mut buf, self.timeout)
                .await
            {
                Ok((len, _from)) if len > 0 => {
                    if let Ok(response) = super::message::DhtMessage::decode(&buf[..len])
                        && response.is_response()
                    {
                        trace!("PingTask: node {} responded", hex::encode(node_id));
                        let mut rt = self.ctx.routing_table.write().await;
                        rt.mark_good(&node_id);
                        return;
                    }
                }
                _ => {
                    // Timeout or error — retry if attempts remain.
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

        for target in targets {
            let rt = Arc::clone(&self.ctx.routing_table);
            let result = iterative_find_node(
                &target,
                &self.ctx.self_id,
                &rt,
                &self.ctx.socket,
                &self.ctx.tracker,
                self.ctx.query_timeout,
            )
            .await;

            // Merge discovered nodes into the routing table.
            {
                let discovered_rt = rt.read().await;
                let mut main_rt = self.ctx.routing_table.write().await;
                for node in discovered_rt.all_nodes() {
                    main_rt.insert(node.clone());
                }
            }

            trace!(
                target = %hex::encode(target),
                nodes_contacted = result.nodes_contacted,
                "Bucket refresh lookup completed"
            );
        }
    }

    fn name(&self) -> &'static str {
        "BucketRefreshTask"
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
        let rt = Arc::clone(&self.ctx.routing_table);
        let result = iterative_find_node(
            &self.target_id,
            &self.ctx.self_id,
            &rt,
            &self.ctx.socket,
            &self.ctx.tracker,
            self.ctx.query_timeout,
        )
        .await;

        // Merge discovered nodes.
        {
            let discovered_rt = rt.read().await;
            let mut main_rt = self.ctx.routing_table.write().await;
            for node in discovered_rt.all_nodes() {
                main_rt.insert(node.clone());
            }
        }

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
        let ctx = DhtTaskContext {
            self_id: [0u8; 20],
            routing_table: Arc::new(RwLock::new(RoutingTable::new([0u8; 20]))),
            socket: DhtSocket::new_test(),
            tracker: Arc::new(TransactionTracker::new()),
            query_timeout: Duration::from_secs(10),
        };
        assert_eq!(ctx.self_id, [0u8; 20]);
    }
}
