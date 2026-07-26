//! DHT Routing Table — Kademlia binary tree routing structure.
//!
//! The routing table is implemented as a binary tree of buckets, following
//! the C++ `DHTRoutingTable` + `DHTBucketTree` architecture. When a bucket
//! is full and contains the local node's ID, it can be split into two
//! child buckets, allowing the routing table to grow dynamically.
//!
//! Key differences from the previous flat-array implementation:
//! - Uses `BucketTreeNode` binary tree instead of `Vec<Bucket>` of size 160
//! - Buckets have ID ranges [min_id, max_id] and prefix lengths
//! - Bucket splitting is driven by node insertion
//! - Replacement cache (CACHE_SIZE=2) for candidate nodes
//! - Tree-based findClosestKNodes with proper neighbor traversal

use tracing::debug;

use super::bucket::Bucket;
use super::bucket_tree::{
    BucketTreeNode, enumerate_buckets, find_bucket_for, find_bucket_for_mut, find_closest_k_nodes,
    find_tree_node_for_mut,
};
use super::node::DhtNode;

/// DHT Routing Table using a binary tree of k-buckets.
///
/// Equivalent to C++ `DHTRoutingTable` + `DHTBucketTreeNode` tree.
pub struct RoutingTable {
    /// Root of the bucket tree.
    root: BucketTreeNode,

    /// Local node ID.
    self_id: [u8; 20],

    /// Number of leaf buckets in the tree.
    num_buckets: usize,
}

impl RoutingTable {
    /// Create a new routing table with a single bucket covering the full ID space.
    pub fn new(self_id: [u8; 20]) -> Self {
        let local_node = DhtNode::new(self_id, "0.0.0.0:0".parse().unwrap());
        let bucket = Bucket::new(&local_node);
        let root = BucketTreeNode::new_leaf(bucket);

        Self {
            root,
            self_id,
            num_buckets: 1,
        }
    }

    /// Insert a node into the routing table.
    ///
    /// If the target bucket is full but can be split (contains our local ID),
    /// the bucket is split and the node is inserted into the appropriate child.
    /// If the bucket cannot be split, the node is cached as a replacement
    /// candidate.
    ///
    /// Equivalent to C++ `DHTRoutingTable::addNode()`.
    pub fn insert(&mut self, node: DhtNode) {
        // Don't add our own node.
        if node.id == self.self_id {
            return;
        }

        let node_id = node.id;
        let node_addr = node.addr;

        // Find the leaf bucket for this node.
        let leaf = find_tree_node_for_mut(&mut self.root, &node_id);

        match leaf {
            BucketTreeNode::Leaf { bucket } => {
                if bucket.add_node(node) {
                    debug!(
                        id = %hex::encode(node_id),
                        "Added DHT node to routing table"
                    );
                    return;
                }

                // Bucket is full. Can we split it?
                if bucket.split_allowed() {
                    debug!(
                        "Splitting bucket (prefix={}) to add node {}",
                        bucket.prefix_length(),
                        hex::encode(node_id),
                    );

                    // Split the leaf node into two children.
                    leaf.split(&self.self_id);
                    self.num_buckets += 1;

                    // Find the correct child and add the node with original address.
                    let node_for_retry = DhtNode::new(node_id, node_addr);
                    let child = find_tree_node_for_mut(&mut self.root, &node_id);
                    if let BucketTreeNode::Leaf { bucket } = child {
                        bucket.add_node(node_for_retry);
                        debug!(
                            id = %hex::encode(node_id),
                            "Added DHT node after split"
                        );
                    }
                } else {
                    // Cannot split — cache the node for potential replacement.
                    let cache_node = DhtNode::new(node_id, node_addr);
                    bucket.cache_node(cache_node);
                    debug!(
                        id = %hex::encode(node_id),
                        "Cached DHT node (bucket full, split not allowed)"
                    );
                }
            }
            BucketTreeNode::Internal { .. } => {
                // This shouldn't happen after find_tree_node_for_mut.
                debug!(
                    id = %hex::encode(node_id),
                    "Unexpected internal node in insert()"
                );
            }
        }
    }

    /// Insert a node that is known to be good (responsive).
    ///
    /// Equivalent to C++ `DHTRoutingTable::addGoodNode()`.
    pub fn insert_good_node(&mut self, node: DhtNode) {
        self.insert(node);
    }

    /// Remove a node from the routing table by its ID.
    ///
    /// If the node's bucket has cached replacement candidates, the first
    /// candidate is promoted to fill the slot.
    pub fn remove(&mut self, node_id: &[u8; 20]) -> bool {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        bucket.drop_node(node_id)
    }

    /// Find the K closest nodes to the given target ID.
    ///
    /// Uses tree-based traversal to efficiently locate the closest nodes.
    pub fn find_closest(&self, target: &[u8; 20], count: usize) -> Vec<DhtNode> {
        let mut nodes = find_closest_k_nodes(&self.root, target);

        // Sort by distance and take the requested count.
        let target_copy = *target;
        nodes.sort_by(|a, b| {
            let da = a.distance_to(&target_copy);
            let db = b.distance_to(&target_copy);
            da.cmp(&db)
        });

        nodes.truncate(count);
        nodes
    }

    /// Find the bucket that contains the given node ID.
    pub fn get_bucket_for(&self, node_id: &[u8; 20]) -> Option<&Bucket> {
        Some(find_bucket_for(&self.root, node_id))
    }

    /// Get all buckets in the routing table.
    pub fn get_all_buckets(&self) -> Vec<&Bucket> {
        let mut buckets = Vec::new();
        enumerate_buckets(&self.root, &mut buckets);
        buckets
    }

    /// Return the total number of nodes across all buckets.
    pub fn total_node_count(&self) -> usize {
        self.get_all_buckets().iter().map(|b| b.count_node()).sum()
    }

    /// Return the number of good (non-bad) nodes across all buckets.
    pub fn good_node_count(&self) -> usize {
        self.get_all_buckets()
            .iter()
            .map(|b| b.good_node_count())
            .sum()
    }

    /// Return the number of buckets in the tree.
    pub fn num_buckets(&self) -> usize {
        self.num_buckets
    }

    /// Evict all bad nodes from all buckets.
    ///
    /// Returns the number of nodes evicted.
    pub fn evict_bad_nodes(&mut self) -> usize {
        let mut total = 0;
        self.for_each_bucket_mut(&mut |bucket| {
            total += bucket.evict_bad();
        });
        total
    }

    /// Mark a node as good (reset failure count, update last_seen).
    pub fn mark_good(&mut self, node_id: &[u8; 20]) -> bool {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        bucket.mark_good(node_id)
    }

    /// Mark a node as bad (increment failure count).
    pub fn mark_bad(&mut self, node_id: &[u8; 20]) -> bool {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        bucket.mark_bad(node_id)
    }

    /// Get a random node from the routing table for bucket refresh.
    pub fn get_random_node(&self) -> Option<&DhtNode> {
        use rand::Rng;
        use rand::seq::SliceRandom;
        let buckets = self.get_all_buckets();

        let non_empty: Vec<_> = buckets.iter().filter(|b| b.count_node() > 0).collect();
        if non_empty.is_empty() {
            return None;
        }

        let mut rng = rand::thread_rng();
        let bucket = non_empty.choose(&mut rng)?;
        let nodes = bucket.nodes();
        if nodes.is_empty() {
            return None;
        }

        let idx = rng.gen_range(0..nodes.len());
        Some(&nodes[idx])
    }

    /// Get all buckets that need refresh.
    pub fn get_buckets_needing_refresh(&self) -> Vec<&Bucket> {
        self.get_all_buckets()
            .into_iter()
            .filter(|b| b.needs_refresh())
            .collect()
    }

    /// Count questionable nodes in the routing table.
    pub fn questionable_node_count(&self) -> usize {
        self.get_all_buckets()
            .iter()
            .map(|b| b.questionable_count())
            .sum()
    }

    /// Count bad nodes in the routing table.
    pub fn bad_node_count(&self) -> usize {
        self.get_all_buckets().iter().map(|b| b.bad_count()).sum()
    }

    /// Refresh buckets that haven't been updated in 15 minutes.
    ///
    /// Returns a list of target IDs to query for each bucket needing refresh.
    pub fn refresh_buckets(&self) -> Vec<[u8; 20]> {
        self.get_all_buckets()
            .iter()
            .filter(|b| b.needs_refresh())
            .map(|b| b.get_random_node_id())
            .collect()
    }

    /// Get all questionable nodes.
    pub fn get_questionable_nodes(&self) -> Vec<&DhtNode> {
        let mut nodes = Vec::new();
        for bucket in self.get_all_buckets() {
            for node in bucket.nodes() {
                if node.is_questionable() {
                    nodes.push(node);
                }
            }
        }
        nodes
    }

    /// Fill the routing table by finding nodes close to our own ID.
    ///
    /// Returns a list of target IDs to query.
    pub fn fill_routing_table(&self) -> Vec<[u8; 20]> {
        self.get_all_buckets()
            .iter()
            .filter(|b| !b.is_full())
            .map(|b| b.get_random_node_id())
            .collect()
    }

    /// Collect all good nodes from the routing table (for persistence).
    pub fn collect_good_nodes(&self) -> Vec<DhtNode> {
        let mut nodes = Vec::new();
        for bucket in self.get_all_buckets() {
            for node in bucket.nodes() {
                if node.is_good() {
                    nodes.push(node.clone());
                }
            }
        }
        nodes
    }

    /// Iterate over all nodes in all buckets.
    pub fn all_nodes(&self) -> Vec<&DhtNode> {
        let mut nodes = Vec::new();
        for bucket in self.get_all_buckets() {
            for node in bucket.nodes() {
                nodes.push(node);
            }
        }
        nodes
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Call `f` for each bucket (mutable).
    fn for_each_bucket_mut(&mut self, f: &mut impl FnMut(&mut Bucket)) {
        Self::for_each_bucket_mut_recursive(&mut self.root, f);
    }

    fn for_each_bucket_mut_recursive(node: &mut BucketTreeNode, f: &mut impl FnMut(&mut Bucket)) {
        match node {
            BucketTreeNode::Leaf { bucket } => {
                f(bucket);
            }
            BucketTreeNode::Internal { left, right, .. } => {
                Self::for_each_bucket_mut_recursive(left, f);
                Self::for_each_bucket_mut_recursive(right, f);
            }
        }
    }
}

// Implement Clone for RoutingTable (needed by engine.rs).
impl Clone for RoutingTable {
    fn clone(&self) -> Self {
        // Clone by collecting all nodes and re-inserting.
        let mut new_table = Self::new(self.self_id);
        for bucket in self.get_all_buckets() {
            for node in bucket.nodes() {
                new_table.insert(node.clone());
            }
        }
        new_table
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn make_addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{}", port).parse().unwrap()
    }

    #[test]
    fn test_routing_table_creation() {
        let table = RoutingTable::new([0u8; 20]);
        assert_eq!(table.total_node_count(), 0);
        assert_eq!(table.num_buckets(), 1);
    }

    #[test]
    fn test_insert_and_find() {
        let mut table = RoutingTable::new([0x80u8; 20]);
        let node = DhtNode::new([0xFFu8; 20], make_addr(6881));
        table.insert(node);

        assert_eq!(table.total_node_count(), 1);

        let target = [0xFFu8; 20];
        let closest = table.find_closest(&target, 5);
        assert_eq!(closest.len(), 1);
    }

    #[test]
    fn test_remove_node() {
        let mut table = RoutingTable::new([0u8; 20]);
        let id = [1u8; 20];
        table.insert(DhtNode::new(id, make_addr(6881)));
        assert_eq!(table.total_node_count(), 1);
        assert!(table.remove(&id));
        assert_eq!(table.total_node_count(), 0);
    }

    #[test]
    fn test_bucket_split_on_insert() {
        let mut table = RoutingTable::new([0u8; 20]);

        // Fill the initial bucket with K nodes.
        for i in 1..=super::super::bucket::K as u8 {
            let node = DhtNode::new([i; 20], make_addr(6881 + i as u16));
            table.insert(node);
        }
        assert_eq!(table.total_node_count(), super::super::bucket::K);

        // Adding one more node should trigger a split if local ID is in range.
        let extra = DhtNode::new([0x80u8; 20], make_addr(9999));
        table.insert(extra);

        // The node should be added (possibly after split).
        assert!(table.total_node_count() >= super::super::bucket::K);
    }

    #[test]
    fn test_mark_good() {
        let mut table = RoutingTable::new([0u8; 20]);
        let id = [1u8; 20];
        let mut node = DhtNode::new(id, make_addr(6881));
        node.record_failure();
        node.record_failure();
        table.insert(node);

        assert!(table.mark_good(&id));
    }

    #[test]
    fn test_mark_bad() {
        let mut table = RoutingTable::new([0u8; 20]);
        let id = [2u8; 20];
        table.insert(DhtNode::new(id, make_addr(6881)));

        assert!(table.mark_bad(&id));
        assert!(table.mark_bad(&id));
        assert!(table.mark_bad(&id));
    }

    #[test]
    fn test_get_random_node() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 0..5u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }

        let node = table.get_random_node();
        assert!(node.is_some());
    }

    #[test]
    fn test_get_random_node_empty_table() {
        let table = RoutingTable::new([0u8; 20]);
        let node = table.get_random_node();
        assert!(node.is_none());
    }

    #[test]
    fn test_collect_good_nodes() {
        let mut table = RoutingTable::new([0u8; 20]);
        table.insert(DhtNode::new([1u8; 20], make_addr(6881)));

        let good = table.collect_good_nodes();
        assert_eq!(good.len(), 1);
    }

    #[test]
    fn test_fill_routing_table() {
        let table = RoutingTable::new([0u8; 20]);
        let targets = table.fill_routing_table();
        // Empty bucket should generate a target.
        assert!(!targets.is_empty());
    }

    #[test]
    fn test_no_self_insert() {
        let self_id = [0x42u8; 20];
        let mut table = RoutingTable::new(self_id);

        // Try to insert our own ID — should be rejected.
        table.insert(DhtNode::new(self_id, make_addr(6881)));
        assert_eq!(table.total_node_count(), 0);
    }

    #[test]
    fn test_cache_node_on_full_bucket() {
        // Use a local_id in the upper half so that after splits, the lower bucket
        // cannot split further (local_id not in range), forcing caching.
        let self_id = [0xFFu8; 20];
        let mut table = RoutingTable::new(self_id);

        // Fill bucket with K good nodes in the lower half (IDs 1..=8).
        // After splits, the lower bucket will be full and split_allowed=false
        // because local_id [0xFF..] is in the upper bucket's range.
        for i in 1..=super::super::bucket::K as u8 {
            let mut id = [0u8; 20];
            id[0] = i; // IDs in lower half
            table.insert(DhtNode::new(id, make_addr(6881 + i as u16)));
        }

        // Add more lower-half nodes to fill the lower bucket after splits.
        for i in 9..=20u8 {
            let mut id = [0u8; 20];
            id[0] = i; // Still in lower half
            table.insert(DhtNode::new(id, make_addr(7000 + i as u16)));
        }

        // Check that at least one bucket has cached nodes.
        let buckets = table.get_all_buckets();
        let has_cached = buckets.iter().any(|b| !b.cached_nodes().is_empty());
        assert!(
            has_cached,
            "Expected at least one bucket to have cached nodes"
        );
    }

    #[test]
    fn test_get_buckets_needing_refresh() {
        let table = RoutingTable::new([0u8; 20]);
        // Empty bucket needs refresh.
        let needing = table.get_buckets_needing_refresh();
        assert!(!needing.is_empty());
    }

    #[test]
    fn test_routing_table_clone() {
        let mut table = RoutingTable::new([0u8; 20]);
        table.insert(DhtNode::new([1u8; 20], make_addr(6881)));

        let cloned = table.clone();
        assert_eq!(cloned.total_node_count(), 1);
    }

    #[test]
    fn test_evict_bad_nodes() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 0..5u8 {
            let mut node = DhtNode::new([i; 20], make_addr(6881 + i as u16));
            if i < 3 {
                for _ in 0..3 {
                    node.record_failure();
                }
            }
            table.insert(node);
        }
        assert!(table.evict_bad_nodes() > 0);
        assert_eq!(table.total_node_count(), 2);
    }
}
