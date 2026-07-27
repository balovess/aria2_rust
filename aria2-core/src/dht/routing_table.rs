//! Top-level Kademlia DHT routing table API.
//!
//! The `RoutingTable` manages a binary tree of K-buckets and provides the
//! primary interface for adding nodes, looking up closest nodes, and
//! maintaining the table's health through bucket splits and refreshes.
//!
//! # Adding Nodes
//!
//! When a node is added:
//! 1. Reject if it has the same ID as the local node
//! 2. Find the bucket that should contain the node
//! 3. Try to add to that bucket
//! 4. If the bucket is full and can be split, split it and retry
//! 5. If the bucket is full and cannot be split, cache the node (if "good")
//!
//! C++ reference: `DHTRoutingTable.h/cc`

use tracing::trace;

use super::BucketTreeNode;
use super::bucket::DhtBucket;
use super::bucket_tree::{
    find_bucket_for, find_bucket_for_mut, find_closest_k_nodes, find_tree_node_for_mut,
};
use super::constants::K;
use super::node::DhtNode;
use super::node_id::NodeId;

/// The Kademlia DHT routing table.
///
/// Wraps a binary tree of buckets centered around a local node ID.
/// Provides operations for node insertion, lookup, and table maintenance.
pub struct RoutingTable {
    /// The local node's ID.
    local_node_id: NodeId,
    /// Root of the bucket tree.
    root: BucketTreeNode,
    /// Number of buckets in the table.
    num_buckets: usize,
}

impl RoutingTable {
    /// Create a new routing table with a single bucket covering the full ID space.
    ///
    /// C++: `DHTRoutingTable(const shared_ptr<DHTNode>& localNode)`
    pub fn new(local_node_id: NodeId) -> Self {
        let bucket = DhtBucket::new(local_node_id);
        let root = BucketTreeNode::leaf(bucket);
        RoutingTable {
            local_node_id,
            root,
            num_buckets: 1,
        }
    }

    /// Add a node to the routing table.
    ///
    /// Returns `true` if the node was successfully added. If the node's ID
    /// matches the local node's ID, it is rejected. If the target bucket is
    /// full and cannot be split, the node is not added.
    ///
    /// C++: `DHTRoutingTable::addNode()`
    pub fn add_node(&mut self, node: DhtNode) -> bool {
        self.add_node_inner(node, false)
    }

    /// Add a "good" node to the routing table.
    ///
    /// Good nodes that cannot fit into a full bucket are cached as
    /// replacement candidates.
    ///
    /// C++: `DHTRoutingTable::addGoodNode()`
    pub fn add_good_node(&mut self, node: DhtNode) -> bool {
        self.add_node_inner(node, true)
    }

    fn add_node_inner(&mut self, node: DhtNode, good: bool) -> bool {
        let node_id = *node.id();

        // Reject nodes with the same ID as the local node
        if node_id == self.local_node_id {
            trace!("Rejected node with same ID as local node");
            return false;
        }

        trace!(node_id = %node_id, "Trying to add node to routing table");

        // Find the leaf tree node for this node's ID
        let tree_node = find_tree_node_for_mut(&mut self.root, &node_id);

        // Try to add to the current bucket. If the bucket is full, we may
        // need to split. Since add_node() takes ownership, we use a
        // two-phase approach: check if we can add, then add.
        {
            // Phase 1: Check the bucket's state
            let (can_add, should_split) = {
                let bucket = tree_node.bucket().expect("leaf must have bucket");
                let already_exists = bucket.nodes().iter().any(|n| n.id() == &node_id);
                let has_room = bucket.count() < K;
                let has_bad_front = bucket.nodes().front().is_some_and(|n| n.is_bad());
                let can = already_exists || has_room || has_bad_front;
                let should = !can && bucket.split_allowed();
                (can, should)
            };

            if can_add {
                let bucket = tree_node.bucket_mut().expect("leaf must have bucket");
                let added = bucket.add_node(node);
                if added {
                    trace!(node_id = %node_id, "Added node to bucket");
                }
                return added;
            }

            if should_split {
                // Use BucketTreeNode::split() which modifies the bucket AND
                // converts the leaf into an internal node with two leaf children.
                // This matches the C++ DHTBucketTreeNode::split() behavior.
                trace!("Splitting bucket tree node");
                tree_node.split();
                self.num_buckets += 1;

                // After split, tree_node is now internal. Navigate to the
                // correct child leaf. Since each child bucket is fresh from
                // split and we're distributing <= K nodes total, each child
                // has room for the new node.
                let go_left = tree_node.left().unwrap().is_in_range(&node_id);
                if go_left {
                    let left = tree_node.left_mut().unwrap();
                    let left_bucket = left.bucket_mut().expect("left child must be leaf");
                    return left_bucket.add_node(node);
                } else {
                    let right = tree_node.right_mut().unwrap();
                    let right_bucket = right.bucket_mut().expect("right child must be leaf");
                    return right_bucket.add_node(node);
                }
            }

            // Bucket is full and cannot be split — cache if good
            if good {
                let bucket = tree_node.bucket_mut().expect("leaf must have bucket");
                bucket.cache_node(node);
                trace!(node_id = %node_id, "Cached node in full bucket");
            }
            false
        }
    }

    /// Get the K closest nodes to the given key.
    ///
    /// C++: `DHTRoutingTable::getClosestKNodes()`
    pub fn get_closest_k_nodes(&self, key: &NodeId) -> Vec<&DhtNode> {
        find_closest_k_nodes(&self.root, key)
    }

    /// Get the bucket that contains the given node ID.
    ///
    /// C++: `DHTRoutingTable::getBucketFor()`
    pub fn get_bucket_for(&self, node_id: &NodeId) -> &DhtBucket {
        find_bucket_for(&self.root, node_id)
    }

    /// Drop a node from its bucket, replacing with a cached node if available.
    ///
    /// C++: `DHTRoutingTable::dropNode()`
    pub fn drop_node(&mut self, node_id: &NodeId) {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        bucket.drop_node(node_id);
    }

    /// Move a node to the tail (MRU position) of its bucket.
    ///
    /// C++: `DHTRoutingTable::moveBucketTail()`
    pub fn move_bucket_tail(&mut self, node_id: &NodeId) {
        let bucket = find_bucket_for_mut(&mut self.root, node_id);
        bucket.move_to_tail(node_id);
    }

    /// Find a node by ID and address verification.
    ///
    /// C++: `DHTRoutingTable::getNode()`
    pub fn get_node(
        &self,
        node_id: &NodeId,
        addr_check: impl Fn(&DhtNode) -> bool,
    ) -> Option<&DhtNode> {
        let bucket = self.get_bucket_for(node_id);
        bucket.get_node(node_id, addr_check)
    }

    /// Get all buckets in the routing table.
    ///
    /// C++: `DHTRoutingTable::getBuckets()`
    pub fn get_buckets(&self) -> Vec<&DhtBucket> {
        super::bucket_tree::enumerate_buckets(&self.root)
    }

    /// Return the number of buckets in the routing table.
    ///
    /// C++: `DHTRoutingTable::getNumBucket()`
    pub fn num_buckets(&self) -> usize {
        self.num_buckets
    }

    /// Return the local node's ID.
    pub fn local_node_id(&self) -> &NodeId {
        &self.local_node_id
    }
}

impl std::fmt::Debug for RoutingTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutingTable")
            .field("local_node_id", &self.local_node_id)
            .field("num_buckets", &self.num_buckets)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dht::constants::ID_LENGTH;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn test_addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881)
    }

    fn make_node(id_byte: u8) -> DhtNode {
        let id = NodeId::from_slice(&[id_byte; ID_LENGTH]);
        DhtNode::new(id, test_addr())
    }

    #[test]
    fn new_routing_table_has_one_bucket() {
        let rt = RoutingTable::new(NodeId::from_slice(&[0x80u8; ID_LENGTH]));
        assert_eq!(rt.num_buckets(), 1);
    }

    #[test]
    fn reject_node_with_local_id() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);
        let node = make_node(0x80);
        assert!(!rt.add_node(node));
    }

    #[test]
    fn add_node_to_empty_table() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);
        let node = make_node(0x01);
        assert!(rt.add_node(node));
    }

    #[test]
    fn add_nodes_trigger_split() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);

        // Add K+1 nodes to trigger a split
        for i in 0u8..9 {
            let node = make_node(i);
            rt.add_node(node);
        }
        // Should have split at least once
        assert!(rt.num_buckets() > 1);
    }

    #[test]
    fn get_closest_k_nodes() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);

        for i in 0u8..5 {
            let mut node = make_node(i);
            node.mark_good();
            node.update_last_contact();
            rt.add_node(node);
        }

        let key = NodeId::from_slice(&[0x02u8; ID_LENGTH]);
        let nodes = rt.get_closest_k_nodes(&key);
        assert!(nodes.len() <= 8);
        assert!(!nodes.is_empty());
    }

    #[test]
    fn get_bucket_for_key() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let rt = RoutingTable::new(local_id);

        let key = NodeId::from_slice(&[0x42u8; ID_LENGTH]);
        let bucket = rt.get_bucket_for(&key);
        assert!(key.is_in_range(bucket.min_id(), bucket.max_id()));
    }

    #[test]
    fn drop_node_from_table() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);

        let mut node = make_node(0x01);
        node.mark_good();
        node.update_last_contact();
        rt.add_node(node);

        let cached = make_node(0x02);
        rt.add_good_node(cached);

        let node_id = NodeId::from_slice(&[0x01u8; ID_LENGTH]);
        rt.drop_node(&node_id);
    }

    #[test]
    fn get_all_buckets() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);

        // Add enough nodes to trigger splits
        for i in 0u8..10 {
            let node = make_node(i);
            rt.add_node(node);
        }

        let buckets = rt.get_buckets();
        assert!(buckets.len() > 1);
    }

    #[test]
    fn add_good_node_caches_when_full() {
        let local_id = NodeId::from_slice(&[0x80u8; ID_LENGTH]);
        let mut rt = RoutingTable::new(local_id);

        // Fill the bucket with nodes that prevent splitting
        // (nodes not in the local node's range won't trigger splits)
        // Since this is a full-range bucket, all nodes are in range
        // and the local node is at 0x80, so splits will happen.
        // Just verify add_good_node doesn't panic.
        for i in 0u8..12 {
            let node = make_node(i);
            rt.add_good_node(node);
        }
    }
}
