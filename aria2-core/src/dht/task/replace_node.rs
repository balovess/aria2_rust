//! DHT node replacement task.

use tracing::{debug, trace};

use super::super::node::DhtNode;
use super::DhtTask;

/// A DHT node replacement task that pings the least-recently-seen node
/// in a bucket. If the ping fails, the node is replaced with a new
/// candidate from the replacement cache.
///
/// C++: `DHTReplaceNodeTask` - pings the LRU node; on timeout, replaces
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

    /// Handle a ping timeout - replace the target with the candidate.
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
