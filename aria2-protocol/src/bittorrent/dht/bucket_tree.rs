//! DHT bucket tree — binary tree routing table structure.
//!
//! The C++ implementation uses `DHTBucketTreeNode` as a binary tree where leaf
//! nodes hold buckets and internal nodes hold min/max ID ranges. When a bucket
//! is full and contains our local node ID, it can be split into two child
//! buckets. This is the core of the Kademlia routing table structure per BEP 5.
//!
//! The Rust version uses `Box` for tree ownership (replacing C++ `unique_ptr`).
//! Buckets are owned directly (not behind `Arc`) since the tree is the sole
//! owner, allowing mutable access through the tree structure.

use super::bucket::Bucket;
use super::node::DhtNode;

/// K-bucket constant: maximum nodes per bucket.
const K: usize = 8;

/// A node in the DHT routing table's binary tree.
///
/// Leaf nodes hold a [`Bucket`] containing up to K [`DhtNode`]s.
/// Internal nodes hold left/right children and the min/max ID range
/// they collectively cover.
///
/// This is the Rust equivalent of C++ `DHTBucketTreeNode`.
pub enum BucketTreeNode {
    /// Internal (branch) node with two children.
    Internal {
        left: Box<BucketTreeNode>,
        right: Box<BucketTreeNode>,
        /// Minimum ID in this subtree (inclusive).
        min_id: [u8; 20],
        /// Maximum ID in this subtree (inclusive).
        max_id: [u8; 20],
    },
    /// Leaf node holding a bucket.
    Leaf { bucket: Bucket },
}

impl BucketTreeNode {
    /// Create a new leaf node wrapping the given bucket.
    pub fn new_leaf(bucket: Bucket) -> Self {
        Self::Leaf { bucket }
    }

    /// Create a new internal node from two children.
    ///
    /// The min/max ID range is derived from the children.
    fn new_internal(left: Box<BucketTreeNode>, right: Box<BucketTreeNode>) -> Self {
        let min_id = left.min_id();
        let max_id = right.max_id();
        Self::Internal {
            left,
            right,
            min_id,
            max_id,
        }
    }

    /// Returns `true` if this is a leaf node.
    pub fn is_leaf(&self) -> bool {
        matches!(self, Self::Leaf { .. })
    }

    /// Returns the minimum ID covered by this subtree.
    pub fn min_id(&self) -> [u8; 20] {
        match self {
            Self::Internal { min_id, .. } => *min_id,
            Self::Leaf { bucket } => *bucket.min_id(),
        }
    }

    /// Returns the maximum ID covered by this subtree.
    pub fn max_id(&self) -> [u8; 20] {
        match self {
            Self::Internal { max_id, .. } => *max_id,
            Self::Leaf { bucket } => *bucket.max_id(),
        }
    }

    /// Returns the child (left or right) that contains the given key.
    ///
    /// Returns `None` if this is a leaf node.
    fn dig(&self, key: &[u8; 20]) -> Option<&BucketTreeNode> {
        match self {
            Self::Internal { left, right, .. } => {
                if left.is_in_range(key) {
                    Some(left)
                } else {
                    Some(right)
                }
            }
            Self::Leaf { .. } => None,
        }
    }

    /// Returns the mutable child (left or right) that contains the given key.
    fn dig_mut(&mut self, key: &[u8; 20]) -> Option<&mut BucketTreeNode> {
        match self {
            Self::Internal { left, right, .. } => {
                if left.is_in_range(key) {
                    Some(left)
                } else {
                    Some(right)
                }
            }
            Self::Leaf { .. } => None,
        }
    }

    /// Returns `true` if `key` falls within this node's [min, max] range.
    pub fn is_in_range(&self, key: &[u8; 20]) -> bool {
        let min = self.min_id();
        let max = self.max_id();
        // key >= min AND key <= max (lexicographic comparison)
        key.as_slice() >= min.as_slice() && key.as_slice() <= max.as_slice()
    }

    /// Get a reference to the bucket if this is a leaf node.
    pub fn bucket(&self) -> Option<&Bucket> {
        match self {
            Self::Leaf { bucket } => Some(bucket),
            Self::Internal { .. } => None,
        }
    }

    /// Get a mutable reference to the bucket if this is a leaf node.
    pub fn bucket_mut(&mut self) -> Option<&mut Bucket> {
        match self {
            Self::Leaf { bucket } => Some(bucket),
            Self::Internal { .. } => None,
        }
    }

    /// Get the left child (internal nodes only).
    pub fn left(&self) -> Option<&BucketTreeNode> {
        match self {
            Self::Internal { left, .. } => Some(left),
            Self::Leaf { .. } => None,
        }
    }

    /// Get the right child (internal nodes only).
    pub fn right(&self) -> Option<&BucketTreeNode> {
        match self {
            Self::Internal { right, .. } => Some(right),
            Self::Leaf { .. } => None,
        }
    }

    /// Split this leaf node's bucket into two child buckets.
    ///
    /// This converts the leaf node into an internal node with two children:
    /// - Left: the existing bucket (mutated in-place to become left half)
    /// - Right: the new bucket returned by `Bucket::split()`
    ///
    /// Panics if called on an internal node.
    pub fn split(&mut self, local_id: &[u8; 20]) {
        match self {
            Self::Leaf { bucket } => {
                // Check if splitting is allowed.
                if !bucket.split_allowed() {
                    tracing::debug!("Bucket split rejected: not allowed");
                    return;
                }

                // Split the bucket in-place: self becomes the left half,
                // right_bucket is the new right half.
                let right_bucket = bucket.split();

                // Create the two child leaf nodes.
                // We need to extract the mutated left bucket since we're about to
                // replace self with an Internal node.
                let left_bucket = std::mem::replace(
                    bucket,
                    Bucket::new_for_range(0, [0u8; 20], [0xFFu8; 20], *local_id),
                );

                let left_node = Box::new(BucketTreeNode::new_leaf(left_bucket));
                let right_node = Box::new(BucketTreeNode::new_leaf(right_bucket));

                *self = Self::new_internal(left_node, right_node);

                tracing::debug!(
                    "Bucket split: left prefix={}, right prefix={}",
                    match self.left() {
                        Some(BucketTreeNode::Leaf { bucket }) => bucket.prefix_length(),
                        _ => 0,
                    },
                    match self.right() {
                        Some(BucketTreeNode::Leaf { bucket }) => bucket.prefix_length(),
                        _ => 0,
                    }
                );
            }
            Self::Internal { .. } => {
                panic!("split() called on non-leaf node");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tree-level query functions (equivalent to C++ dht namespace functions)
// ---------------------------------------------------------------------------

/// Find the leaf tree node whose bucket range contains `key`.
///
/// Equivalent to C++ `dht::findTreeNodeFor()`.
pub fn find_tree_node_for<'a>(root: &'a BucketTreeNode, key: &[u8; 20]) -> &'a BucketTreeNode {
    match root {
        BucketTreeNode::Leaf { .. } => root,
        BucketTreeNode::Internal { .. } => {
            let child = root.dig(key).expect("internal node must have children");
            find_tree_node_for(child, key)
        }
    }
}

/// Find the mutable leaf tree node whose bucket range contains `key`.
pub fn find_tree_node_for_mut<'a>(
    root: &'a mut BucketTreeNode,
    key: &[u8; 20],
) -> &'a mut BucketTreeNode {
    match root {
        BucketTreeNode::Leaf { .. } => root,
        BucketTreeNode::Internal { .. } => {
            let child = root.dig_mut(key).expect("internal node must have children");
            find_tree_node_for_mut(child, key)
        }
    }
}

/// Find the bucket whose range contains `key`.
///
/// Equivalent to C++ `dht::findBucketFor()`.
pub fn find_bucket_for<'a>(root: &'a BucketTreeNode, key: &[u8; 20]) -> &'a Bucket {
    let leaf = find_tree_node_for(root, key);
    match leaf {
        BucketTreeNode::Leaf { bucket } => bucket,
        BucketTreeNode::Internal { .. } => unreachable!("find_tree_node_for returned internal"),
    }
}

/// Find the mutable bucket whose range contains `key`.
pub fn find_bucket_for_mut<'a>(root: &'a mut BucketTreeNode, key: &[u8; 20]) -> &'a mut Bucket {
    let leaf = find_tree_node_for_mut(root, key);
    match leaf {
        BucketTreeNode::Leaf { bucket } => bucket,
        BucketTreeNode::Internal { .. } => unreachable!("find_tree_node_for returned internal"),
    }
}

/// Collect up to K closest good nodes to `key` from the routing tree.
///
/// This is the tree-based equivalent of C++ `dht::findClosestKNodes()`.
/// It traverses the tree to find the leaf bucket containing `key`,
/// then collects nodes from the parent's subtree and walks upward
/// until K nodes are found.
pub fn find_closest_k_nodes(root: &BucketTreeNode, key: &[u8; 20]) -> Vec<DhtNode> {
    let mut nodes = Vec::with_capacity(K);
    collect_closest_from_all_buckets(root, key, &mut nodes);
    nodes
}

/// Collect closest nodes by walking all leaf buckets.
///
/// We collect good nodes from all buckets, sort by distance to key,
/// and take the K closest.
fn collect_closest_from_all_buckets(
    root: &BucketTreeNode,
    key: &[u8; 20],
    nodes: &mut Vec<DhtNode>,
) {
    let mut all_buckets = Vec::new();
    enumerate_buckets(root, &mut all_buckets);

    for bucket in &all_buckets {
        for node in bucket.nodes() {
            if !node.is_bad() {
                nodes.push(node.clone());
            }
        }
    }

    // Sort by distance to key (ascending = closest first).
    let key_copy = *key;
    nodes.sort_by(|a, b| {
        let da = a.distance_to(&key_copy);
        let db = b.distance_to(&key_copy);
        da.cmp(&db)
    });

    nodes.truncate(K);
}

/// Enumerate all leaf buckets in the tree (in-order traversal).
///
/// Equivalent to C++ `dht::enumerateBucket()`.
pub fn enumerate_buckets<'a>(root: &'a BucketTreeNode, buckets: &mut Vec<&'a Bucket>) {
    match root {
        BucketTreeNode::Leaf { bucket } => {
            buckets.push(bucket);
        }
        BucketTreeNode::Internal { left, right, .. } => {
            enumerate_buckets(left, buckets);
            enumerate_buckets(right, buckets);
        }
    }
}

/// Count the total number of leaf buckets in the tree.
pub fn count_buckets(root: &BucketTreeNode) -> usize {
    match root {
        BucketTreeNode::Leaf { .. } => 1,
        BucketTreeNode::Internal { left, right, .. } => count_buckets(left) + count_buckets(right),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn make_local_node() -> DhtNode {
        DhtNode::new([0u8; 20], "127.0.0.1:6881".parse::<SocketAddr>().unwrap())
    }

    #[test]
    fn test_leaf_node_creation() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        let node = BucketTreeNode::new_leaf(bucket);

        assert!(node.is_leaf());
        assert!(node.bucket().is_some());
        assert!(node.left().is_none());
        assert!(node.right().is_none());
    }

    #[test]
    fn test_is_in_range() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        let node = BucketTreeNode::new_leaf(bucket);

        // The initial bucket covers the full ID space [0x00..0xFF]
        let zero_id = [0u8; 20];
        let max_id = [0xFFu8; 20];
        assert!(node.is_in_range(&zero_id));
        assert!(node.is_in_range(&max_id));
    }

    #[test]
    fn test_find_tree_node_for() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        let root = BucketTreeNode::new_leaf(bucket);

        let key = [0x42u8; 20];
        let found = find_tree_node_for(&root, &key);
        assert!(found.is_leaf());
    }

    #[test]
    fn test_enumerate_buckets_single() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        let root = BucketTreeNode::new_leaf(bucket);

        let mut buckets = Vec::new();
        enumerate_buckets(&root, &mut buckets);
        assert_eq!(buckets.len(), 1);
    }

    #[test]
    fn test_count_buckets() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        let root = BucketTreeNode::new_leaf(bucket);
        assert_eq!(count_buckets(&root), 1);
    }
}
