//! DHT bucket refresh task.

use tracing::debug;

use super::super::constants::BUCKET_REFRESH_INTERVAL_SECS;
use super::super::node_id::NodeId;
use super::super::routing_table::RoutingTable;
use super::DhtTask;

/// A DHT bucket refresh task that identifies stale buckets and starts
/// node lookups for them.
///
/// C++: `DHTBucketRefreshTask` - iterates routing table buckets and
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
        let buckets = routing_table.get_buckets();
        for bucket in &buckets {
            if self.force_refresh
                || bucket.time_since_last_update().as_secs() > BUCKET_REFRESH_INTERVAL_SECS
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
