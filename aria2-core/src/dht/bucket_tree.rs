//! Binary tree of K-buckets forming the Kademlia routing table structure.
//!
//! The `BucketTree` is a binary tree where leaf nodes hold `DhtBucket`
//! instances. Internal nodes store the min/max ID range of their subtrees.
//! The tree is highly unbalanced — it grows by splitting only the bucket
//! that contains the local node, ensuring that bucket can always accept
//! new nodes close to the local ID.
//!
//! # Tree Navigation
//!
//! - `find_bucket_for(key)` — returns the bucket containing `key`
//! - `find_closest_k_nodes(key)` — returns up to K nodes closest to `key`
//! - `enumerate_buckets()` — visits all buckets in order
//!
//! C++ reference: `DHTBucketTree.h/cc`

use super::bucket::DhtBucket;
use super::constants::K;
use super::node::DhtNode;
use super::node_id::NodeId;

/// A node in the bucket tree (either internal or leaf).
///
/// Leaf nodes hold a `DhtBucket`. Internal nodes have left and right
/// children. After a split, a leaf becomes an internal node with two
/// leaf children.
///
/// C++: `DHTBucketTreeNode`
pub enum BucketTreeNode {
    /// Internal node with two children.
    Internal {
        /// Inclusive lower bound of this subtree's ID range.
        min_id: NodeId,
        /// Inclusive upper bound of this subtree's ID range.
        max_id: NodeId,
        /// Left child (covers the upper ID range after split).
        left: Box<BucketTreeNode>,
        /// Right child (covers the lower ID range after split).
        right: Box<BucketTreeNode>,
    },
    /// Leaf node holding a bucket.
    Leaf { bucket: DhtBucket },
}

impl BucketTreeNode {
    /// Create a leaf node wrapping the given bucket.
    pub fn leaf(bucket: DhtBucket) -> Self {
        BucketTreeNode::Leaf { bucket }
    }

    /// Check if this is a leaf node.
    pub fn is_leaf(&self) -> bool {
        matches!(self, BucketTreeNode::Leaf { .. })
    }

    /// Get the min ID of this subtree's range.
    pub fn min_id(&self) -> &NodeId {
        match self {
            BucketTreeNode::Internal { min_id, .. } => min_id,
            BucketTreeNode::Leaf { bucket } => bucket.min_id(),
        }
    }

    /// Get the max ID of this subtree's range.
    pub fn max_id(&self) -> &NodeId {
        match self {
            BucketTreeNode::Internal { max_id, .. } => max_id,
            BucketTreeNode::Leaf { bucket } => bucket.max_id(),
        }
    }

    /// Check if a key falls within this subtree's ID range.
    ///
    /// C++: `DHTBucketTreeNode::isInRange()`
    pub fn is_in_range(&self, key: &NodeId) -> bool {
        key.is_in_range(self.min_id(), self.max_id())
    }

    /// Navigate to the child that contains the key.
    ///
    /// Returns `Some(left_or_right)` for internal nodes, `None` for leaves.
    ///
    /// C++: `DHTBucketTreeNode::dig()`
    pub fn dig(&self, key: &NodeId) -> Option<&BucketTreeNode> {
        match self {
            BucketTreeNode::Internal { left, right, .. } => {
                if left.is_in_range(key) {
                    Some(left)
                } else {
                    Some(right)
                }
            }
            BucketTreeNode::Leaf { .. } => None,
        }
    }

    /// Get the bucket if this is a leaf node.
    pub fn bucket(&self) -> Option<&DhtBucket> {
        match self {
            BucketTreeNode::Leaf { bucket } => Some(bucket),
            _ => None,
        }
    }

    /// Get the bucket mutably if this is a leaf node.
    pub fn bucket_mut(&mut self) -> Option<&mut DhtBucket> {
        match self {
            BucketTreeNode::Leaf { bucket } => Some(bucket),
            _ => None,
        }
    }

    /// Split the leaf node into an internal node with two leaf children.
    ///
    /// This consumes the current leaf, splits its bucket, and creates
    /// an internal node with the left (upper) and right (lower) children.
    ///
    /// C++: `DHTBucketTreeNode::split()`
    ///
    /// # Panics
    ///
    /// Panics if called on an internal node.
    pub fn split(&mut self) {
        // Extract the bucket from the leaf, replacing self with an internal node.
        let old_self = std::mem::replace(
            self,
            BucketTreeNode::Leaf {
                bucket: DhtBucket::new(NodeId::ZERO), // placeholder
            },
        );

        let bucket = match old_self {
            BucketTreeNode::Leaf { bucket } => bucket,
            _ => panic!("split() called on non-leaf node"),
        };

        // Split the bucket — self becomes left (upper), r_bucket is right (lower)
        let mut left_bucket = bucket;
        let right_bucket = left_bucket.split();

        // The internal node's range spans both children
        let internal_min = *right_bucket.min_id();
        let internal_max = *left_bucket.max_id();

        *self = BucketTreeNode::Internal {
            min_id: internal_min,
            max_id: internal_max,
            left: Box::new(BucketTreeNode::Leaf {
                bucket: left_bucket,
            }),
            right: Box::new(BucketTreeNode::Leaf {
                bucket: right_bucket,
            }),
        };
    }

    /// Get the left child (for internal nodes).
    pub fn left(&self) -> Option<&BucketTreeNode> {
        match self {
            BucketTreeNode::Internal { left, .. } => Some(left),
            _ => None,
        }
    }

    /// Get the right child (for internal nodes).
    pub fn right(&self) -> Option<&BucketTreeNode> {
        match self {
            BucketTreeNode::Internal { right, .. } => Some(right),
            _ => None,
        }
    }

    /// Get the left child mutably (for internal nodes).
    pub fn left_mut(&mut self) -> Option<&mut BucketTreeNode> {
        match self {
            BucketTreeNode::Internal { left, .. } => Some(left),
            _ => None,
        }
    }

    /// Get the right child mutably (for internal nodes).
    pub fn right_mut(&mut self) -> Option<&mut BucketTreeNode> {
        match self {
            BucketTreeNode::Internal { right, .. } => Some(right),
            _ => None,
        }
    }
}

// ===========================================================================
// Free functions for tree traversal (matching C++ dht namespace)
// ===========================================================================

/// Find the leaf tree node that contains the given key.
///
/// C++: `dht::findTreeNodeFor()`
pub fn find_tree_node_for<'a>(root: &'a BucketTreeNode, key: &NodeId) -> &'a BucketTreeNode {
    if root.is_leaf() {
        return root;
    }
    match root.dig(key) {
        Some(child) => find_tree_node_for(child, key),
        None => root,
    }
}

/// Find the leaf tree node that contains the given key (mutable).
pub fn find_tree_node_for_mut<'a>(
    root: &'a mut BucketTreeNode,
    key: &NodeId,
) -> &'a mut BucketTreeNode {
    if root.is_leaf() {
        return root;
    }
    // Determine which child to descend into before borrowing mutably
    let go_left = root.left().map_or(false, |l| l.is_in_range(key));
    if go_left {
        find_tree_node_for_mut(root.left_mut().unwrap(), key)
    } else {
        find_tree_node_for_mut(root.right_mut().unwrap(), key)
    }
}

/// Find the bucket that contains the given key.
///
/// C++: `dht::findBucketFor()`
pub fn find_bucket_for<'a>(root: &'a BucketTreeNode, key: &NodeId) -> &'a DhtBucket {
    let leaf = find_tree_node_for(root, key);
    leaf.bucket().expect("leaf node must have a bucket")
}

/// Find the bucket that contains the given key (mutable).
pub fn find_bucket_for_mut<'a>(root: &'a mut BucketTreeNode, key: &NodeId) -> &'a mut DhtBucket {
    let leaf = find_tree_node_for_mut(root, key);
    leaf.bucket_mut().expect("leaf node must have a bucket")
}

/// Collect up to K closest nodes to the given key.
///
/// Implements the Kademlia "find node" algorithm:
/// 1. Find the leaf bucket for the key
/// 2. Collect good nodes from that bucket
/// 3. If fewer than K, traverse siblings upward
/// 4. Trim to K results
///
/// C++: `dht::findClosestKNodes()`
pub fn find_closest_k_nodes<'a>(root: &'a BucketTreeNode, key: &NodeId) -> Vec<&'a DhtNode> {
    let mut nodes = Vec::with_capacity(K);
    find_closest_k_nodes_into(&mut nodes, root, key);
    nodes
}

fn find_closest_k_nodes_into<'a>(
    nodes: &mut Vec<&'a DhtNode>,
    root: &'a BucketTreeNode,
    key: &NodeId,
) {
    if K <= nodes.len() {
        return;
    }

    let leaf = find_tree_node_for(root, key);

    // If the leaf IS the root (single bucket tree), just collect from it
    if std::ptr::eq(leaf, root) {
        collect_good_nodes(nodes, leaf.bucket().unwrap());
        return;
    }

    // Otherwise, collect from the parent's subtree
    // We need to find the parent. Since we don't have parent pointers,
    // we use a different approach: collect from the leaf first, then
    // collect from all other buckets.
    collect_good_nodes(nodes, leaf.bucket().unwrap());

    if nodes.len() < K {
        collect_all_good_nodes(nodes, root, leaf.bucket().unwrap());
    }

    // Trim to K
    nodes.truncate(K);
}

/// Collect good nodes from a bucket into the result vector.
fn collect_good_nodes<'a>(nodes: &mut Vec<&'a DhtNode>, bucket: &'a DhtBucket) {
    for node in bucket.good_nodes() {
        nodes.push(node);
    }
}

/// Collect good nodes from all buckets except the excluded one.
fn collect_all_good_nodes<'a>(
    nodes: &mut Vec<&'a DhtNode>,
    tree: &'a BucketTreeNode,
    exclude: &DhtBucket,
) {
    match tree {
        BucketTreeNode::Leaf { bucket } => {
            if !std::ptr::eq(bucket as *const _, exclude as *const _) {
                collect_good_nodes(nodes, bucket);
            }
        }
        BucketTreeNode::Internal { left, right, .. } => {
            if nodes.len() < K {
                collect_all_good_nodes(nodes, left, exclude);
            }
            if nodes.len() < K {
                collect_all_good_nodes(nodes, right, exclude);
            }
        }
    }
}

/// Enumerate all buckets in the tree.
///
/// C++: `dht::enumerateBucket()`
pub fn enumerate_buckets(root: &BucketTreeNode) -> Vec<&DhtBucket> {
    let mut buckets = Vec::new();
    enumerate_buckets_into(&mut buckets, root);
    buckets
}

fn enumerate_buckets_into<'a>(buckets: &mut Vec<&'a DhtBucket>, node: &'a BucketTreeNode) {
    match node {
        BucketTreeNode::Leaf { bucket } => {
            buckets.push(bucket);
        }
        BucketTreeNode::Internal { left, right, .. } => {
            enumerate_buckets_into(buckets, left);
            enumerate_buckets_into(buckets, right);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::constants::ID_LENGTH;
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881)
    }

    fn make_node(id_byte: u8) -> DhtNode {
        let id = NodeId::from_slice(&[id_byte; ID_LENGTH]);
        DhtNode::new(id, test_addr())
    }

    #[test]
    fn leaf_node_holds_bucket() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let bucket = DhtBucket::new(local_id);
        let node = BucketTreeNode::leaf(bucket);

        assert!(node.is_leaf());
        assert!(node.bucket().is_some());
        // Full-range bucket contains all IDs
        assert!(node.is_in_range(&NodeId::from_slice(&[0x00u8; ID_LENGTH])));
        assert!(node.is_in_range(&NodeId::from_slice(&[0x80u8; ID_LENGTH])));
        assert!(node.is_in_range(&NodeId::from_slice(&[0xFFu8; ID_LENGTH])));
    }

    #[test]
    fn find_bucket_in_single_leaf() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let bucket = DhtBucket::new(local_id);
        let root = BucketTreeNode::leaf(bucket);

        let key = NodeId::from_slice(&[0x42u8; ID_LENGTH]);
        let found = find_bucket_for(&root, &key);
        assert_eq!(found.prefix_length(), 0);
    }

    #[test]
    fn split_creates_internal_node() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let bucket = DhtBucket::new(local_id);
        let mut root = BucketTreeNode::leaf(bucket);

        root.split();
        assert!(!root.is_leaf());
        assert!(root.left().unwrap().is_leaf());
        assert!(root.right().unwrap().is_leaf());
    }

    #[test]
    fn find_bucket_after_split() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let bucket = DhtBucket::new(local_id);
        let mut root = BucketTreeNode::leaf(bucket);
        root.split();

        // Key in upper half
        let upper_key = NodeId::from_slice(&[0xFFu8; ID_LENGTH]);
        let upper_bucket = find_bucket_for(&root, &upper_key);
        assert!(upper_key.is_in_range(upper_bucket.min_id(), upper_bucket.max_id()));

        // Key in lower half
        let lower_key = NodeId::from_slice(&[0x01u8; ID_LENGTH]);
        let lower_bucket = find_bucket_for(&root, &lower_key);
        assert!(lower_key.is_in_range(lower_bucket.min_id(), lower_bucket.max_id()));
    }

    #[test]
    fn find_closest_k_nodes_returns_nodes() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut bucket = DhtBucket::new(local_id);
        for i in 0..3u8 {
            let mut node = make_node(i);
            node.mark_good();
            node.update_last_contact();
            bucket.add_node(node);
        }
        let root = BucketTreeNode::leaf(bucket);

        let key = NodeId::from_slice(&[0x01u8; ID_LENGTH]);
        let nodes = find_closest_k_nodes(&root, &key);
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn enumerate_buckets_single_leaf() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let bucket = DhtBucket::new(local_id);
        let root = BucketTreeNode::leaf(bucket);

        let buckets = enumerate_buckets(&root);
        assert_eq!(buckets.len(), 1);
    }

    #[test]
    fn enumerate_buckets_after_split() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let bucket = DhtBucket::new(local_id);
        let mut root = BucketTreeNode::leaf(bucket);
        root.split();

        let buckets = enumerate_buckets(&root);
        assert_eq!(buckets.len(), 2);
    }
}
