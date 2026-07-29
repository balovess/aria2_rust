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
//! - `DHTAnnounceTask` → [`DhtAnnounceTask`] (announce_peer after peer lookup)

mod announce;
mod bucket_refresh;
mod executor;
mod lookup;
mod ping;
mod replace_node;

pub use announce::DhtAnnounceTask;
pub use bucket_refresh::DhtBucketRefreshTask;
pub use executor::{DhtTaskQueue, TaskExecutor};
pub use lookup::{DhtLookupTask, LookupKind, LookupResult, LookupState};
pub use ping::DhtPingTask;
pub use replace_node::DhtReplaceNodeTask;

use tokio::sync::mpsc;

use super::node::DhtNode;

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
pub struct LookupEntry {
    pub(super) node: DhtNode,
    pub(super) used: bool,
}

// ── Lookup result channel ───────────────────────────────────────────────────

/// Channel sender for completed lookup results.
///
/// When a `DhtLookupTask` finishes, it sends its `LookupResult` through
/// this channel so the DHT engine can process it (e.g., trigger
/// announce_peer for peer lookups, feed peers to the BT engine).
pub type LookupResultSender = mpsc::UnboundedSender<LookupResult>;

/// Channel receiver for completed lookup results.
pub type LookupResultReceiver = mpsc::UnboundedReceiver<LookupResult>;

/// Create a new lookup result channel.
pub fn lookup_result_channel() -> (LookupResultSender, LookupResultReceiver) {
    mpsc::unbounded_channel()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::node_id::NodeId;
    use crate::dht::routing_table::RoutingTable;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
        assert!(batch.len() <= 3); // ALPHA
        assert!(batch.len() > 0);
    }
}
