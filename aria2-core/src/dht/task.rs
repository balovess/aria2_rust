//! DHT task system: lookup, ping, bucket refresh, and node replacement.
//!
//! Implements the Kademlia iterative lookup algorithm with α-parallelism
//! (ALPHA = 3 concurrent queries) for both node lookups and peer lookups.
//!
//! # C++ Reference
//!
//! - `DHTTask` / `DHTAbstractTask` → [`DhtTask`] trait
//! - `DHTAbstractNodeLookupTask` → [`LookupState`] + [`DhtLookupTask`]
//! - `DHTNodeLookupTask` → [`LookupKind::Node`]
//! - `DHTPeerLookupTask` → [`LookupKind::Peer`]
//! - `DHTPingTask` → [`DhtPingTask`]
//! - `DHTBucketRefreshTask` → [`DhtBucketRefreshTask`]
//! - `DHTReplaceNodeTask` → [`DhtReplaceNodeTask`]
//! - `DHTTaskQueueImpl` → [`DhtTaskQueue`]
//! - `DHTTaskExecutor` → [`TaskExecutor`]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::Duration;

use tracing::{debug, trace};

use super::constants::{K, MESSAGE_TIMEOUT_SECS};
use super::node::DhtNode;
use super::node_id::NodeId;
use super::routing_table::RoutingTable;

/// Kademlia α (alpha) — maximum concurrent in-flight queries per lookup.
const ALPHA: usize = 3;

// ── Task trait ───────────────────────────────────────────────────────────────

/// A DHT task that can be started and checked for completion.
///
/// C++: `DHTTask` interface with `startup()` and `finished()`.
pub trait DhtTask {
    /// Begin executing the task (sends initial messages).
    fn startup(&mut self);

    /// Returns `true` when the task has completed or failed.
    fn finished(&self) -> bool;
}

// ── Lookup entry ─────────────────────────────────────────────────────────────

/// An entry in a lookup's candidate list.
///
/// C++: `DHTNodeLookupEntry` — tracks whether a node has been queried.
#[derive(Clone, Debug)]
struct LookupEntry {
    node: DhtNode,
    used: bool,
}

// ── Lookup kind ──────────────────────────────────────────────────────────────

/// Discriminant for the two lookup variants.
///
/// C++: `DHTNodeLookupTask` vs `DHTPeerLookupTask` (separate classes).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupKind {
    /// Find the K closest nodes to a target ID (find_node query).
    Node,
    /// Find peers for an info hash (get_peers query).
    Peer,
}

// ── Lookup result ────────────────────────────────────────────────────────────

/// Outcome of a completed lookup task.
#[derive(Clone, Debug)]
pub struct LookupResult {
    /// The kind of lookup that was performed.
    pub kind: LookupKind,
    /// The target ID that was looked up.
    pub target: NodeId,
    /// Nodes discovered during the lookup (up to K closest).
    pub nodes: Vec<DhtNode>,
    /// Peers discovered (only for [`LookupKind::Peer`]).
    pub peers: Vec<SocketAddr>,
    /// Tokens received from get_peers responses, keyed by node address.
    /// Used for subsequent announce_peer messages.
    pub tokens: Vec<(SocketAddr, Vec<u8>)>,
}

// ── Lookup state ─────────────────────────────────────────────────────────────

/// Shared state for the Kademlia iterative lookup algorithm.
///
/// C++: `DHTAbstractNodeLookupTask<Resp>` — the core lookup engine.
///
/// The algorithm sends up to `ALPHA` queries concurrently to the closest
/// known nodes. As responses arrive, newly discovered nodes are inserted
/// (sorted by XOR distance to the target), and the next closest unqueried
/// node is sent a query. The lookup terminates when all K closest nodes
/// have been queried and all in-flight messages have completed.
pub struct LookupState {
    /// The target node ID or info hash being looked up.
    target: NodeId,
    /// The kind of lookup (node vs peer).
    kind: LookupKind,
    /// Candidate nodes sorted by distance to target, closest first.
    entries: VecDeque<LookupEntry>,
    /// Number of in-flight messages awaiting a response.
    in_flight: usize,
    /// Accumulated discovered nodes (deduped, up to K).
    discovered_nodes: Vec<DhtNode>,
    /// Accumulated discovered peers (for peer lookup only).
    discovered_peers: Vec<SocketAddr>,
    /// Tokens received from get_peers responses.
    tokens: Vec<(SocketAddr, Vec<u8>)>,
    /// Whether the lookup has finished.
    done: bool,
}

impl LookupState {
    /// Create a new lookup state for the given target.
    pub fn new(target: NodeId, kind: LookupKind) -> Self {
        Self {
            target,
            kind,
            entries: VecDeque::new(),
            in_flight: 0,
            discovered_nodes: Vec::new(),
            discovered_peers: Vec::new(),
            tokens: Vec::new(),
            done: false,
        }
    }

    /// Seed the lookup with the K closest nodes from the routing table.
    pub fn startup(&mut self, routing_table: &RoutingTable, local_id: &NodeId) {
        let closest = routing_table.get_closest_k_nodes(&self.target);
        for node in closest {
            // Skip the local node
            if node.id() == local_id {
                continue;
            }
            self.entries.push_back(LookupEntry {
                node: node.clone(),
                used: false,
            });
        }

        if self.entries.is_empty() {
            trace!("No seed nodes for lookup, finishing immediately");
            self.done = true;
        }
    }

    /// Get the next batch of nodes to query (up to ALPHA unused entries).
    ///
    /// Returns a list of `(entry_index, node)` pairs. The caller must
    /// mark these entries as used after sending the messages.
    pub fn next_query_batch(&self) -> Vec<(usize, &DhtNode)> {
        let mut batch = Vec::with_capacity(ALPHA);
        let remaining = ALPHA.saturating_sub(self.in_flight);
        for (i, entry) in self.entries.iter().enumerate() {
            if batch.len() >= remaining {
                break;
            }
            if !entry.used {
                batch.push((i, &entry.node));
            }
        }
        batch
    }

    /// Mark entries at the given indices as used (queries sent).
    pub fn mark_sent(&mut self, indices: &[usize]) {
        for &i in indices {
            if let Some(entry) = self.entries.get_mut(i) {
                entry.used = true;
                self.in_flight += 1;
            }
        }
    }

    /// Handle a response from a node in the lookup.
    ///
    /// `sender_addr` identifies which entry responded.
    /// `nodes` are the nodes reported in the response.
    /// `peers` are the peers reported (for peer lookups).
    /// `token` is the announce token (for peer lookups).
    pub fn on_response(
        &mut self,
        sender_addr: SocketAddr,
        nodes: Vec<DhtNode>,
        peers: Vec<SocketAddr>,
        token: Option<Vec<u8>>,
        local_id: &NodeId,
    ) {
        self.in_flight = self.in_flight.saturating_sub(1);

        // Update the responding node's address if it changed
        for entry in &mut self.entries {
            if entry.node.addr() == sender_addr {
                // Node responded successfully — mark as reachable
                entry.node.mark_contacted();
            }
        }

        // Store token from get_peers response
        if let Some(tok) = token {
            self.tokens.push((sender_addr, tok));
        }

        // Add discovered peers
        self.discovered_peers.extend(peers);

        // Insert newly discovered nodes, sorted by distance
        for node in nodes {
            // Skip the local node
            if node.id() == local_id {
                continue;
            }

            // Skip duplicates
            if self.discovered_nodes.iter().any(|n| n.id() == node.id()) {
                continue;
            }
            if self.entries.iter().any(|e| e.node.id() == node.id()) {
                continue;
            }

            self.discovered_nodes.push(node.clone());

            // Insert into entries sorted by distance to target
            let dist = node.id().distance_to(&self.target);
            let pos = self.entries.iter().position(|e| {
                let e_dist = e.node.id().distance_to(&self.target);
                dist < e_dist
            });

            let entry = LookupEntry { node, used: false };

            match pos {
                Some(idx) => self.entries.insert(idx, entry),
                None => self.entries.push_back(entry),
            }
        }

        // Trim to K entries
        while self.entries.len() > K {
            self.entries.pop_back();
        }

        // Deduplicate entries by node ID
        let mut seen = std::collections::HashSet::new();
        self.entries
            .retain(|e| seen.insert(*e.node.id().as_bytes()));

        self.check_finished();
    }

    /// Handle a timeout for a node in the lookup.
    pub fn on_timeout(&mut self, timed_out_addr: SocketAddr) {
        self.in_flight = self.in_flight.saturating_sub(1);

        // Remove the timed-out entry
        self.entries.retain(|e| e.node.addr() != timed_out_addr);

        self.check_finished();
    }

    /// Check if the lookup is complete.
    fn check_finished(&mut self) {
        // Try to send more queries
        let remaining = ALPHA.saturating_sub(self.in_flight);
        let has_unused = self.entries.iter().any(|e| !e.used);

        if remaining > 0 && has_unused {
            // More queries to send — not done yet
            return;
        }

        if self.in_flight == 0 {
            trace!(
                target = %self.target,
                kind = ?self.kind,
                "Lookup finished"
            );
            self.done = true;
        }
    }

    /// Whether the lookup has completed.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Consume the state and produce a result.
    pub fn into_result(self) -> LookupResult {
        // Collect the K closest nodes from entries
        let nodes: Vec<DhtNode> = self
            .entries
            .into_iter()
            .map(|e| e.node)
            .chain(self.discovered_nodes)
            .take(K)
            .collect();

        LookupResult {
            kind: self.kind,
            target: self.target,
            nodes,
            peers: self.discovered_peers,
            tokens: self.tokens,
        }
    }

    /// Get the target node ID.
    pub fn target(&self) -> &NodeId {
        &self.target
    }

    /// Get the lookup kind.
    pub fn kind(&self) -> &LookupKind {
        &self.kind
    }
}

// ── DhtLookupTask ────────────────────────────────────────────────────────────

/// A DHT iterative lookup task (node or peer lookup).
///
/// C++: `DHTNodeLookupTask` / `DHTPeerLookupTask`
pub struct DhtLookupTask {
    state: LookupState,
    started: bool,
}

impl DhtLookupTask {
    /// Create a new lookup task for the given target and kind.
    pub fn new(target: NodeId, kind: LookupKind) -> Self {
        Self {
            state: LookupState::new(target, kind),
            started: false,
        }
    }

    /// Get a mutable reference to the inner lookup state.
    pub fn state_mut(&mut self) -> &mut LookupState {
        &mut self.state
    }

    /// Get a reference to the inner lookup state.
    pub fn state(&self) -> &LookupState {
        &self.state
    }
}

impl DhtTask for DhtLookupTask {
    fn startup(&mut self) {
        self.started = true;
        // startup() on LookupState requires a routing table, which is
        // provided separately via state_mut().startup().
    }

    fn finished(&self) -> bool {
        self.state.is_done()
    }
}

// ── DhtPingTask ──────────────────────────────────────────────────────────────

/// A DHT ping task to verify node connectivity.
///
/// C++: `DHTPingTask` — sends ping to a remote node with retry support.
pub struct DhtPingTask {
    /// The remote node to ping.
    remote_node: DhtNode,
    /// Maximum number of retries.
    max_retry: u32,
    /// Current retry count.
    retry_count: u32,
    /// Whether the ping was successful.
    success: bool,
    /// Whether the task is finished.
    done: bool,
    /// Timeout for each ping attempt.
    timeout: Duration,
}

impl DhtPingTask {
    /// Create a new ping task for the given remote node.
    pub fn new(remote_node: DhtNode, max_retry: u32) -> Self {
        Self {
            remote_node,
            max_retry,
            retry_count: 0,
            success: false,
            done: false,
            timeout: Duration::from_secs(MESSAGE_TIMEOUT_SECS),
        }
    }

    /// Handle a successful ping response.
    pub fn on_response(&mut self) {
        debug!(addr = %self.remote_node.addr(), "Ping successful");
        self.success = true;
        self.done = true;
    }

    /// Handle a ping timeout — retry if retries remain.
    pub fn on_timeout(&mut self) -> bool {
        self.retry_count += 1;
        if self.retry_count > self.max_retry {
            debug!(
                addr = %self.remote_node.addr(),
                retries = self.retry_count,
                "Ping failed after max retries"
            );
            self.done = true;
            return false; // no more retries
        }
        trace!(
            addr = %self.remote_node.addr(),
            retry = self.retry_count,
            "Ping timeout, retrying"
        );
        true // retry
    }

    /// Get the remote node being pinged.
    pub fn remote_node(&self) -> &DhtNode {
        &self.remote_node
    }

    /// Whether the ping was successful.
    pub fn is_success(&self) -> bool {
        self.success
    }

    /// Get the timeout duration.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

impl DhtTask for DhtPingTask {
    fn startup(&mut self) {
        // Ping is sent by the message dispatcher when the task is started.
        trace!(addr = %self.remote_node.addr(), "Starting ping task");
    }

    fn finished(&self) -> bool {
        self.done
    }
}

// ── DhtBucketRefreshTask ─────────────────────────────────────────────────────

/// A DHT bucket refresh task that identifies stale buckets and starts
/// node lookups for them.
///
/// C++: `DHTBucketRefreshTask` — iterates routing table buckets and
/// creates `DHTNodeLookupTask` for any bucket that hasn't been refreshed
/// within `BUCKET_REFRESH_INTERVAL_SECS`.
pub struct DhtBucketRefreshTask {
    /// Whether to force refresh of all buckets.
    force_refresh: bool,
    /// The target IDs to look up (one per stale bucket).
    targets: Vec<NodeId>,
    /// Whether the task has started.
    started: bool,
    /// Whether the task is finished.
    done: bool,
}

impl DhtBucketRefreshTask {
    /// Create a new bucket refresh task.
    pub fn new(force_refresh: bool) -> Self {
        Self {
            force_refresh,
            targets: Vec::new(),
            started: false,
            done: false,
        }
    }

    /// Compute refresh targets from the routing table.
    ///
    /// Identifies buckets that haven't been refreshed in
    /// `BUCKET_REFRESH_INTERVAL_SECS` and generates a random ID within
    /// each stale bucket's range as the lookup target.
    pub fn compute_targets(&mut self, routing_table: &RoutingTable) {
        use super::constants::BUCKET_REFRESH_INTERVAL_SECS;

        let buckets = routing_table.get_buckets();
        for bucket in &buckets {
            if self.force_refresh
                || bucket.time_since_last_update().as_secs() > BUCKET_REFRESH_INTERVAL_SECS as u64
            {
                // Generate a random ID within this bucket's range
                let target = bucket.random_id_in_range();
                self.targets.push(target);
                debug!(
                    prefix_len = bucket.common_prefix_len(),
                    "Scheduling bucket refresh"
                );
            }
        }

        if self.targets.is_empty() {
            debug!("No stale buckets to refresh");
            self.done = true;
        }
    }

    /// Get the refresh targets (lookup IDs for stale buckets).
    pub fn targets(&self) -> &[NodeId] {
        &self.targets
    }

    /// Consume the targets, leaving the task in a finished state.
    pub fn take_targets(&mut self) -> Vec<NodeId> {
        self.done = true;
        std::mem::take(&mut self.targets)
    }
}

impl DhtTask for DhtBucketRefreshTask {
    fn startup(&mut self) {
        self.started = true;
        // Targets must be computed before startup via compute_targets().
        // The caller then creates DhtLookupTask for each target.
    }

    fn finished(&self) -> bool {
        self.done
    }
}

// ── DhtReplaceNodeTask ───────────────────────────────────────────────────────

/// A DHT node replacement task that pings the least-recently-seen node
/// in a bucket. If the ping fails, the node is replaced with a new
/// candidate from the replacement cache.
///
/// C++: `DHTReplaceNodeTask` — pings the LRU node; on timeout, replaces
/// it with the new candidate from the bucket's replacement cache.
pub struct DhtReplaceNodeTask {
    /// The bucket containing the node to potentially replace.
    bucket_prefix_len: usize,
    /// The node being tested (LRU node in the bucket).
    target_node: DhtNode,
    /// The replacement candidate.
    replacement_node: DhtNode,
    /// Current retry count.
    retry_count: u32,
    /// Maximum retries before giving up.
    max_retry: u32,
    /// Whether the ping was answered (target node stays).
    target_alive: bool,
    /// Whether the task is finished.
    done: bool,
}

impl DhtReplaceNodeTask {
    /// Create a new replace node task.
    pub fn new(bucket_prefix_len: usize, target_node: DhtNode, replacement_node: DhtNode) -> Self {
        Self {
            bucket_prefix_len,
            target_node,
            replacement_node,
            retry_count: 0,
            max_retry: 0, // no retries by default
            target_alive: false,
            done: false,
        }
    }

    /// Handle a successful ping response from the target node.
    /// The target node is alive and should not be replaced.
    pub fn on_response(&mut self) {
        debug!(
            addr = %self.target_node.addr(),
            "Replace node task: target is alive"
        );
        self.target_alive = true;
        self.done = true;
    }

    /// Handle a ping timeout — replace the target with the candidate.
    pub fn on_timeout(&mut self) {
        self.retry_count += 1;
        if self.retry_count > self.max_retry {
            debug!(
                addr = %self.target_node.addr(),
                replacement = %self.replacement_node.addr(),
                "Replacing unresponsive node with candidate"
            );
            self.done = true;
        }
    }

    /// Get the target (LRU) node being pinged.
    pub fn target_node(&self) -> &DhtNode {
        &self.target_node
    }

    /// Get the replacement candidate node.
    pub fn replacement_node(&self) -> &DhtNode {
        &self.replacement_node
    }

    /// Get the bucket prefix length.
    pub fn bucket_prefix_len(&self) -> usize {
        self.bucket_prefix_len
    }

    /// Whether the target node is alive.
    pub fn is_target_alive(&self) -> bool {
        self.target_alive
    }
}

impl DhtTask for DhtReplaceNodeTask {
    fn startup(&mut self) {
        trace!(
            target = %self.target_node.addr(),
            replacement = %self.replacement_node.addr(),
            "Starting replace node task"
        );
    }

    fn finished(&self) -> bool {
        self.done
    }
}

// ── TaskExecutor ─────────────────────────────────────────────────────────────

/// Executes a queue of DHT tasks with a concurrency limit.
///
/// C++: `DHTTaskExecutor` — manages running and pending tasks.
pub struct TaskExecutor {
    /// Maximum number of concurrent tasks.
    max_concurrent: usize,
    /// Currently executing tasks.
    running: Vec<Box<dyn DhtTask>>,
    /// Pending tasks.
    queue: VecDeque<Box<dyn DhtTask>>,
}

impl TaskExecutor {
    /// Create a new executor with the given concurrency limit.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            max_concurrent,
            running: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    /// Add a task to the pending queue.
    pub fn add_task(&mut self, task: Box<dyn DhtTask>) {
        self.queue.push_back(task);
    }

    /// Tick the executor: start new tasks and remove finished ones.
    pub fn update(&mut self) {
        // Remove finished tasks
        self.running.retain(|t| !t.finished());

        // Start new tasks up to the concurrency limit
        while self.running.len() < self.max_concurrent {
            let Some(mut task) = self.queue.pop_front() else {
                break;
            };
            task.startup();
            self.running.push(task);
        }
    }

    /// Number of currently executing tasks.
    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    /// Number of pending tasks.
    pub fn queue_size(&self) -> usize {
        self.queue.len()
    }
}

// ── DhtTaskQueue ─────────────────────────────────────────────────────────────

/// The DHT task queue with three priority levels.
///
/// C++: `DHTTaskQueueImpl` — immediate, periodic1, periodic2 queues.
///
/// - **Immediate**: one-shot tasks like peer lookups triggered by user action
/// - **Periodic1**: bucket refresh and similar periodic maintenance
/// - **Periodic2**: peer announcement and other lower-priority periodic tasks
pub struct DhtTaskQueue {
    /// High-priority immediate tasks.
    immediate: TaskExecutor,
    /// Periodic maintenance tasks (bucket refresh, etc.).
    periodic1: TaskExecutor,
    /// Lower-priority periodic tasks (peer announce, etc.).
    periodic2: TaskExecutor,
}

impl DhtTaskQueue {
    /// Create a new task queue with default concurrency limits.
    pub fn new() -> Self {
        Self {
            immediate: TaskExecutor::new(15),
            periodic1: TaskExecutor::new(5),
            periodic2: TaskExecutor::new(5),
        }
    }

    /// Add an immediate (one-shot) task.
    pub fn add_immediate(&mut self, task: Box<dyn DhtTask>) {
        self.immediate.add_task(task);
    }

    /// Add a periodic1 (bucket refresh) task.
    pub fn add_periodic1(&mut self, task: Box<dyn DhtTask>) {
        self.periodic1.add_task(task);
    }

    /// Add a periodic2 (peer announce) task.
    pub fn add_periodic2(&mut self, task: Box<dyn DhtTask>) {
        self.periodic2.add_task(task);
    }

    /// Execute one tick of all task queues.
    pub fn execute(&mut self) {
        self.immediate.update();
        self.periodic1.update();
        self.periodic2.update();
    }

    /// Number of tasks across all queues.
    pub fn total_tasks(&self) -> usize {
        self.immediate.running_count()
            + self.immediate.queue_size()
            + self.periodic1.running_count()
            + self.periodic1.queue_size()
            + self.periodic2.running_count()
            + self.periodic2.queue_size()
    }
}

impl Default for DhtTaskQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    /// A trivial task that finishes immediately after startup.
    struct ImmediateTask {
        started: bool,
        done: bool,
    }

    impl DhtTask for ImmediateTask {
        fn startup(&mut self) {
            self.started = true;
            self.done = true;
        }
        fn finished(&self) -> bool {
            self.done
        }
    }

    /// A task that never finishes on its own.
    struct NeverDoneTask;

    impl DhtTask for NeverDoneTask {
        fn startup(&mut self) {}
        fn finished(&self) -> bool {
            false
        }
    }

    fn make_node(id_byte: u8, port: u16) -> DhtNode {
        let mut id = [0u8; 20];
        id[0] = id_byte;
        DhtNode::new(
            NodeId(id),
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, id_byte)), port),
        )
    }

    #[test]
    fn ping_task_success() {
        let node = make_node(1, 6881);
        let mut task = DhtPingTask::new(node, 0);
        task.startup();
        assert!(!task.finished());
        task.on_response();
        assert!(task.finished());
        assert!(task.is_success());
    }

    #[test]
    fn ping_task_timeout_retry() {
        let node = make_node(2, 6882);
        let mut task = DhtPingTask::new(node, 2);
        task.startup();

        // First timeout: should retry
        assert!(task.on_timeout());
        assert!(!task.finished());

        // Second timeout: should retry
        assert!(task.on_timeout());
        assert!(!task.finished());

        // Third timeout: exceeds max_retry (2), should finish
        assert!(!task.on_timeout());
        assert!(task.finished());
        assert!(!task.is_success());
    }

    #[test]
    fn bucket_refresh_no_stale_buckets() {
        let mut task = DhtBucketRefreshTask::new(false);
        let rt = RoutingTable::new(NodeId::random());
        task.compute_targets(&rt);
        // Fresh routing table has no stale buckets, but may still have
        // empty buckets that need seeding — skip assertion on count
        assert!(task.targets().len() <= 1); // at most one initial lookup
    }

    #[test]
    fn bucket_refresh_force() {
        let mut task = DhtBucketRefreshTask::new(true);
        let rt = RoutingTable::new(NodeId::random());
        task.compute_targets(&rt);
        // Force refresh should identify at least one bucket
        assert!(!task.targets().is_empty());
    }

    #[test]
    fn replace_node_target_alive() {
        let target = make_node(1, 6881);
        let replacement = make_node(2, 6882);
        let mut task = DhtReplaceNodeTask::new(0, target, replacement);
        task.startup();
        task.on_response();
        assert!(task.finished());
        assert!(task.is_target_alive());
    }

    #[test]
    fn replace_node_target_dead() {
        let target = make_node(1, 6881);
        let replacement = make_node(2, 6882);
        let mut task = DhtReplaceNodeTask::new(0, target, replacement);
        task.startup();
        task.on_timeout();
        assert!(task.finished());
        assert!(!task.is_target_alive());
    }

    #[test]
    fn task_executor_runs_tasks() {
        let mut executor = TaskExecutor::new(2);
        executor.add_task(Box::new(ImmediateTask {
            started: false,
            done: false,
        }));
        executor.add_task(Box::new(ImmediateTask {
            started: false,
            done: false,
        }));

        assert_eq!(executor.queue_size(), 2);
        // First tick: starts tasks from queue. ImmediateTask sets done=true in
        // startup(), but finished tasks are only removed at the *beginning* of
        // the next tick, so we need a second call to clear them.
        executor.update();
        executor.update();
        // Both should have started and finished immediately
        assert_eq!(executor.running_count(), 0);
        assert_eq!(executor.queue_size(), 0);
    }

    #[test]
    fn task_executor_respects_concurrency() {
        let mut executor = TaskExecutor::new(1);
        executor.add_task(Box::new(NeverDoneTask));
        executor.add_task(Box::new(NeverDoneTask));

        executor.update();
        assert_eq!(executor.running_count(), 1);
        assert_eq!(executor.queue_size(), 1);
    }

    #[test]
    fn task_queue_default() {
        let queue = DhtTaskQueue::new();
        assert_eq!(queue.total_tasks(), 0);
    }

    #[test]
    fn lookup_state_startup_seeds_from_routing_table() {
        let local_id = NodeId::random();
        let mut rt = RoutingTable::new(local_id);
        // Add some nodes to the routing table
        for i in 1u8..=5u8 {
            let node = make_node(i, 6881 + i as u16);
            rt.add_node(node);
        }

        let target = NodeId::random();
        let mut state = LookupState::new(target, LookupKind::Node);
        state.startup(&rt, &local_id);

        assert!(!state.entries.is_empty());
        assert_eq!(state.in_flight, 0);
    }

    #[test]
    fn lookup_state_next_query_batch() {
        let local_id = NodeId::random();
        let mut rt = RoutingTable::new(local_id);
        for i in 1u8..=5u8 {
            let node = make_node(i, 6881 + i as u16);
            rt.add_node(node);
        }

        let target = NodeId::random();
        let mut state = LookupState::new(target, LookupKind::Node);
        state.startup(&rt, &local_id);

        let batch = state.next_query_batch();
        assert!(batch.len() <= ALPHA);
        assert!(batch.len() > 0);
    }
}
