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

    /// Rebind a routed endpoint to the node ID returned by a valid response.
    ///
    /// A peer endpoint may begin answering with a different Kademlia ID. The
    /// old identity must be removed and the responder inserted according to
    /// its actual ID range; it may not belong to the old node's bucket.
    pub(crate) fn replace_node_identity(
        &mut self,
        old_node_id: &[u8; 20],
        replacement: DhtNode,
    ) -> bool {
        let old_bucket = find_bucket_for(&self.root, old_node_id);
        if !old_bucket
            .nodes()
            .iter()
            .any(|node| &node.id == old_node_id)
            || !replacement.is_good()
        {
            return false;
        }

        if old_node_id == replacement.id() {
            return self.mark_good(old_node_id);
        }

        let replacement_id = *replacement.id();
        self.remove(old_node_id);
        self.root.for_each_bucket_mut(&mut |bucket| {
            bucket.remove_cached_node(&replacement_id);
        });
        self.insert(replacement);
        true
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
        use rand::RngExt;
        use rand::seq::IndexedRandom;
        let buckets = self.get_all_buckets();

        let non_empty: Vec<_> = buckets.iter().filter(|b| b.count_node() > 0).collect();
        if non_empty.is_empty() {
            return None;
        }

        let mut rng = rand::rng();
        let bucket = non_empty.choose(&mut rng)?;
        let nodes = bucket.nodes();
        if nodes.is_empty() {
            return None;
        }

        let idx = rng.random_range(0..nodes.len());
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
mod tests;
