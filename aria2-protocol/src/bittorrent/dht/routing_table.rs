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
        let mut node = node;

        loop {
            let leaf = find_tree_node_for_mut(&mut self.root, &node_id);
            node = match leaf {
                BucketTreeNode::Leaf { bucket } => match bucket.try_add_node(node) {
                    Ok(()) => {
                        debug!(
                            id = %hex::encode(node_id),
                            "Added DHT node to routing table"
                        );
                        return;
                    }
                    Err(node) => node,
                },
                BucketTreeNode::Internal { .. } => {
                    debug!(
                        id = %hex::encode(node_id),
                        "Unexpected internal node in insert()"
                    );
                    return;
                }
            };

            let split_allowed = match leaf {
                BucketTreeNode::Leaf { bucket } => bucket.split_allowed(),
                BucketTreeNode::Internal { .. } => unreachable!("leaf lookup returned a branch"),
            };
            if split_allowed {
                let prefix_length = match leaf {
                    BucketTreeNode::Leaf { bucket } => bucket.prefix_length(),
                    BucketTreeNode::Internal { .. } => {
                        unreachable!("leaf lookup returned a branch")
                    }
                };
                debug!(
                    "Splitting bucket (prefix={}) to add node {}",
                    prefix_length,
                    hex::encode(node_id),
                );
                leaf.split(&self.self_id);
                self.num_buckets += 1;
                continue;
            }

            if let BucketTreeNode::Leaf { bucket } = leaf
                && node.is_good()
            {
                bucket.cache_node(node);
            }
            debug!(
                id = %hex::encode(node_id),
                "Cached DHT node (bucket full, split not allowed)"
            );
            return;
        }
    }

    /// Remove a node from the routing table by its ID.
    ///
    /// If the node's bucket has cached replacement candidates, the first
    /// candidate is promoted to fill the slot.
    pub fn remove(&mut self, node_id: &[u8; 20]) -> bool {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        bucket.drop_node(node_id)
    }

    /// Replace a specific node in its bucket with a verified candidate.
    pub fn replace_node(&mut self, node_id: &[u8; 20], replacement: DhtNode) -> bool {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        if !bucket.nodes().iter().any(|node| &node.id == node_id)
            || !bucket
                .cached_nodes()
                .iter()
                .any(|node| node.id == replacement.id)
        {
            return false;
        }
        bucket.remove_cached_node(&replacement.id);
        bucket.replace_node(node_id, replacement)
    }

    /// Find the K closest nodes to the given target ID.
    ///
    /// Uses tree-based traversal to efficiently locate the closest nodes.
    pub fn find_closest(&self, target: &[u8; 20], count: usize) -> Vec<DhtNode> {
        let mut nodes = find_closest_k_nodes(&self.root, target);
        nodes.truncate(count);
        nodes
    }

    /// Find the bucket that contains the given node ID.
    pub fn get_bucket_for(&self, node_id: &[u8; 20]) -> &Bucket {
        find_bucket_for(&self.root, node_id)
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

    /// Return the number of recently verified good nodes across all buckets.
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
        self.root.for_each_bucket_mut(&mut |bucket| {
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
    fn bucket_lookup_returns_a_covering_leaf_for_any_id_range() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 1..=super::super::bucket::K as u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }
        table.insert(DhtNode::new([0x80; 20], make_addr(6999)));

        let low_id = [0u8; 20];
        let high_id = [0xFF; 20];
        assert!(table.get_bucket_for(&low_id).is_in_range(&low_id));
        assert!(table.get_bucket_for(&high_id).is_in_range(&high_id));
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
    fn bucket_split_preserves_discovered_node_verification_state() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 1..=super::super::bucket::K as u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }

        table.insert(DhtNode::unverified([0x80u8; 20], make_addr(9999)));

        assert_eq!(table.total_node_count(), super::super::bucket::K + 1);
        assert_eq!(
            table.good_node_count(),
            super::super::bucket::K,
            "inserting across a bucket split must not promote an unverified node"
        );
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

        for _ in 0..5 {
            assert!(table.mark_bad(&id));
        }
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
    fn full_non_local_bucket_caches_only_verified_replacements() {
        let self_id = [0xFF; 20];
        let mut table = RoutingTable::new(self_id);
        for i in 1..=super::super::bucket::K as u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }

        let candidate_id = [0x40; 20];
        table.insert(DhtNode::unverified(candidate_id, make_addr(6990)));
        assert_eq!(table.num_buckets(), 2);

        let bucket = table.get_bucket_for(&candidate_id);
        assert!(
            !bucket.is_in_range(&self_id),
            "candidate must be in a non-local bucket"
        );
        assert_eq!(bucket.count_node(), super::super::bucket::K);
        assert!(bucket.nodes().iter().all(|node| node.id() != &candidate_id));
        assert!(
            bucket
                .cached_nodes()
                .iter()
                .all(|node| node.id() != &candidate_id),
            "unverified candidates must not enter the replacement cache"
        );

        let verified_id = [0x41; 20];
        table.insert(DhtNode::new(verified_id, make_addr(6991)));
        assert!(
            table
                .get_bucket_for(&verified_id)
                .cached_nodes()
                .iter()
                .any(|node| node.id() == &verified_id),
            "verified candidates must enter the replacement cache"
        );
    }

    #[test]
    fn unverified_candidate_cannot_replace_bad_lru_in_full_non_local_bucket() {
        let self_id = [0xFF; 20];
        let mut table = RoutingTable::new(self_id);
        for i in 1..=super::super::bucket::K as u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }

        let failed_id = [1; 20];
        for _ in 0..5 {
            assert!(table.mark_bad(&failed_id));
        }

        let unverified_id = [0x40; 20];
        table.insert(DhtNode::unverified(unverified_id, make_addr(6990)));

        let bucket = table.get_bucket_for(&unverified_id);
        assert_eq!(bucket.count_node(), super::super::bucket::K);
        assert!(
            bucket.nodes().iter().any(|node| node.id() == &failed_id),
            "insertion must not evict a bad node in favor of an unverified candidate"
        );
        assert!(
            bucket
                .nodes()
                .iter()
                .all(|node| node.id() != &unverified_id),
            "an unverified candidate must not enter a full non-local bucket"
        );
        assert!(
            bucket
                .cached_nodes()
                .iter()
                .all(|node| node.id() != &unverified_id),
            "an unverified candidate must not enter the replacement cache"
        );

        assert_eq!(table.evict_bad_nodes(), 1);
        let bucket = table.get_bucket_for(&unverified_id);
        assert_eq!(bucket.count_node(), super::super::bucket::K - 1);
        assert!(
            bucket
                .nodes()
                .iter()
                .all(|node| node.id() != &unverified_id),
            "bad-node eviction must not promote the rejected unverified candidate"
        );
    }

    #[test]
    fn test_evict_bad_nodes() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 1..=super::super::bucket::K as u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }
        let lower_bad_id = [1; 20];
        let upper_bad_id = [0x80; 20];
        table.insert(DhtNode::new(upper_bad_id, make_addr(6999)));
        assert_eq!(table.num_buckets(), 2);
        for _ in 0..5 {
            assert!(table.mark_bad(&lower_bad_id));
            assert!(table.mark_bad(&upper_bad_id));
        }

        assert_eq!(table.evict_bad_nodes(), 2);
        assert_eq!(table.total_node_count(), super::super::bucket::K - 1);
    }

    #[test]
    fn evicting_bad_node_promotes_the_newest_verified_cached_replacement() {
        let mut table = RoutingTable::new([0xFF; 20]);
        for i in 1..=super::super::bucket::K as u8 {
            table.insert(DhtNode::new([i; 20], make_addr(6881 + i as u16)));
        }

        let older_candidate = [0x40; 20];
        let replacement_id = [0x41; 20];
        table.insert(DhtNode::new(older_candidate, make_addr(6990)));
        table.insert(DhtNode::new(replacement_id, make_addr(6991)));
        let unverified_id = [0x3F; 20];
        table.insert(DhtNode::unverified(unverified_id, make_addr(6989)));

        let failed_id = [1; 20];
        for _ in 0..5 {
            assert!(table.mark_bad(&failed_id));
        }

        assert_eq!(table.evict_bad_nodes(), 1);
        let bucket = table.get_bucket_for(&replacement_id);
        assert_eq!(bucket.count_node(), super::super::bucket::K);
        assert!(
            bucket
                .nodes()
                .iter()
                .any(|node| node.id() == &replacement_id)
        );
        assert!(
            bucket
                .cached_nodes()
                .iter()
                .any(|node| node.id() == &older_candidate),
            "remaining replacement cache: {:?}",
            bucket
                .cached_nodes()
                .iter()
                .map(DhtNode::id)
                .collect::<Vec<_>>()
        );
        assert!(
            bucket
                .nodes()
                .iter()
                .chain(bucket.cached_nodes())
                .all(|node| node.id() != &unverified_id),
            "unverified candidate must not be promoted or retained in the replacement cache"
        );
    }
}
