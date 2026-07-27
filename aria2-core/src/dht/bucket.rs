//! K-bucket: container for up to K nodes in the DHT routing table.
//!
//! A `DhtBucket` holds up to `K` (8) nodes that share a common ID prefix.
//! Nodes are kept in ascending order by last-seen time (MRU at back, LRU at
//! front). When the bucket is full, bad nodes at the front are evicted; if
//! no bad nodes exist, new nodes go to a replacement cache of size
//! `CACHE_SIZE` (2).
//!
//! # Bucket Splitting
//!
//! Buckets can be split in half, creating two child buckets with disjoint
//! ID ranges. Splitting is allowed only if the local node falls within the
//! bucket's range (to ensure one bucket always covers the local node).
//!
//! # Split Algorithm (from C++ DHTBucket::split)
//!
//! Given bucket range [min, max] with prefix_length P:
//! 1. Copy min -> rMin, copy max -> rMax
//! 2. Flip bit P in rMax  (rMax becomes upper bound of lower half)
//! 3. Flip bit P in min   (min becomes lower bound of upper half)
//! 4. Increment prefix_length to P+1
//! 5. Self (left/upper): [min_flipped, max_original]
//! 6. New bucket (right/lower): [rMin_original, rMax_flipped]
//! 7. Redistribute nodes between the two buckets
//!
//! C++ reference: `DHTBucket.h/cc`

use std::collections::VecDeque;
use std::time::Instant;

use tracing::trace;

use super::constants::{CACHE_SIZE, ID_LENGTH, K};
use super::node::DhtNode;
use super::node_id::NodeId;

/// A K-bucket in the Kademlia routing table.
///
/// Each bucket covers a contiguous range of node IDs `[min_id, max_id]`
/// (inclusive). The `prefix_length` indicates how many leading bits are
/// shared by all IDs in this range.
pub struct DhtBucket {
    /// Number of leading bits that are identical for all IDs in this range.
    prefix_length: usize,
    /// Inclusive lower bound of the ID range.
    min_id: NodeId,
    /// Inclusive upper bound of the ID range.
    max_id: NodeId,
    /// The local node's ID (used to decide if splitting is allowed).
    local_node_id: NodeId,
    /// Active nodes in this bucket, sorted LRU (front) to MRU (back).
    nodes: VecDeque<Box<DhtNode>>,
    /// Replacement cache, sorted by most-recently-seen first.
    cached_nodes: VecDeque<Box<DhtNode>>,
    /// Time of last update to this bucket.
    last_updated: Instant,
}

impl DhtBucket {
    /// Create a bucket covering the entire ID space [0x00..0xFF].
    ///
    /// C++: `DHTBucket(const shared_ptr<DHTNode>& localNode)` initializes
    /// `min_` to all-zeros and `max_` to all-ones.
    pub fn new(local_node_id: NodeId) -> Self {
        DhtBucket {
            prefix_length: 0,
            min_id: NodeId::ZERO,
            max_id: NodeId::MAX,
            local_node_id,
            nodes: VecDeque::with_capacity(K),
            cached_nodes: VecDeque::with_capacity(CACHE_SIZE),
            last_updated: Instant::now(),
        }
    }

    /// Create a bucket with a specific ID range.
    ///
    /// C++: `DHTBucket(size_t prefixLength, const unsigned char* max,
    /// const unsigned char* min, ...)`
    pub fn with_range(
        prefix_length: usize,
        max_id: NodeId,
        min_id: NodeId,
        local_node_id: NodeId,
    ) -> Self {
        DhtBucket {
            prefix_length,
            min_id,
            max_id,
            local_node_id,
            nodes: VecDeque::with_capacity(K),
            cached_nodes: VecDeque::with_capacity(CACHE_SIZE),
            last_updated: Instant::now(),
        }
    }

    /// Generate a random node ID within this bucket's range.
    ///
    /// C++: `getRandomNodeID()` — copies the prefix from min_id, then
    /// randomizes the remaining bits.
    pub fn random_node_id(&self) -> NodeId {
        if self.prefix_length == 0 {
            return NodeId::random();
        }
        let mut id = NodeId::random();
        let last_byte_index = (self.prefix_length - 1) / 8;
        // Copy the prefix from min_id (leading bytes that are fully determined)
        id.0[..=last_byte_index].copy_from_slice(&self.min_id.0[..=last_byte_index]);
        id
    }

    /// Check if a node ID falls within this bucket's range.
    ///
    /// C++: `isInRange(const unsigned char* nodeID)` — uses lexicographic
    /// comparison for [min, max] inclusive.
    pub fn is_in_range(&self, id: &NodeId) -> bool {
        id.is_in_range(&self.min_id, &self.max_id)
    }

    /// Check if the local node falls within this bucket's range.
    pub fn contains_local_node(&self) -> bool {
        self.is_in_range(&self.local_node_id)
    }

    /// Try to add a node to this bucket.
    ///
    /// Returns `true` if the node was added (or updated), `false` if the
    /// bucket is full and no bad node can be evicted.
    ///
    /// C++: `addNode()` — if node exists, move to tail; if bucket has room,
    /// append; if full and front is bad, evict front and append; else false.
    pub fn add_node(&mut self, node: DhtNode) -> bool {
        self.notify_update();
        let node_id = *node.id();

        // Check if node already exists in the bucket
        if let Some(pos) = self.nodes.iter().position(|n| n.id() == &node_id) {
            self.nodes.remove(pos);
            self.nodes.push_back(Box::new(node));
            return true;
        }

        // New node
        if self.nodes.len() < K {
            self.nodes.push_back(Box::new(node));
            return true;
        }

        // Bucket full: evict bad node at front (LRU) if possible
        if self.nodes.front().map_or(false, |n| n.is_bad()) {
            self.nodes.pop_front();
            self.nodes.push_back(Box::new(node));
            return true;
        }

        false
    }

    /// Cache a node as a replacement candidate.
    ///
    /// C++: `cacheNode()` — pushes to front of cache, trims to CACHE_SIZE.
    pub fn cache_node(&mut self, node: DhtNode) {
        self.cached_nodes.push_front(Box::new(node));
        if self.cached_nodes.len() > CACHE_SIZE {
            self.cached_nodes.truncate(CACHE_SIZE);
        }
        trace!(
            prefix = self.prefix_length,
            cache_len = self.cached_nodes.len(),
            "Cached DHT node"
        );
    }

    /// Drop a node from the bucket, replacing it with the first cached node.
    ///
    /// C++: `dropNode()` — removes the specified node, promotes the head
    /// of the replacement cache into the bucket.
    pub fn drop_node(&mut self, node_id: &NodeId) {
        if self.cached_nodes.is_empty() {
            return;
        }
        if let Some(pos) = self.nodes.iter().position(|n| n.id() == node_id) {
            self.nodes.remove(pos);
            if let Some(replacement) = self.cached_nodes.pop_front() {
                self.nodes.push_back(replacement);
            }
        }
    }

    /// Move a node to the head (LRU position) of the bucket.
    ///
    /// C++: `moveToHead()`
    pub fn move_to_head(&mut self, node_id: &NodeId) {
        if let Some(pos) = self.nodes.iter().position(|n| n.id() == node_id) {
            let node = self.nodes.remove(pos).unwrap();
            self.nodes.push_front(node);
        }
    }

    /// Move a node to the tail (MRU position) of the bucket.
    ///
    /// C++: `moveToTail()`
    pub fn move_to_tail(&mut self, node_id: &NodeId) {
        if let Some(pos) = self.nodes.iter().position(|n| n.id() == node_id) {
            let node = self.nodes.remove(pos).unwrap();
            self.nodes.push_back(node);
        }
    }

    /// Find a node by ID, address, and port.
    ///
    /// C++: `getNode()` — returns the node only if ID, IP, and port all match.
    pub fn get_node(
        &self,
        node_id: &NodeId,
        addr_check: impl Fn(&DhtNode) -> bool,
    ) -> Option<&DhtNode> {
        self.nodes
            .iter()
            .find(|n| n.id() == node_id && addr_check(n))
            .map(|b| b.as_ref())
    }

    /// Check if splitting this bucket is allowed.
    ///
    /// Splitting is allowed when:
    /// 1. The prefix length hasn't reached the maximum (159 for 20-byte IDs)
    /// 2. The local node falls within this bucket's range
    ///
    /// C++: `splitAllowed()`
    pub fn split_allowed(&self) -> bool {
        self.prefix_length < ID_LENGTH * 8 - 1 && self.contains_local_node()
    }

    /// Split this bucket into two, returning the right (lower) bucket.
    ///
    /// After split:
    /// - Self becomes the LEFT (upper) half: [min_flipped, max_original]
    /// - Returned bucket is the RIGHT (lower) half: [min_original, max_flipped]
    ///
    /// Both buckets have prefix_length incremented by 1.
    /// Nodes are redistributed to the correct half based on their ID.
    ///
    /// C++: `DHTBucket::split()`
    ///
    /// # Panics
    ///
    /// Panics if `split_allowed()` is false.
    pub fn split(&mut self) -> Self {
        assert!(self.split_allowed(), "Bucket split not allowed");

        // C++ split algorithm (traced from DHTBucket.cc):
        //
        //   rMax = max_;                           // copy of original max
        //   rMin = min_;                           // copy of original min
        //   flipBit(rMax, prefixLength_);           // rMax -> upper bound of lower half
        //   flipBit(min_, prefixLength_);           // min_ -> lower bound of upper half
        //   ++prefixLength_;
        //   rBucket = DHTBucket(prefixLength_, rMax, rMin, localNode_);
        //
        // Example with prefix=0, min=0x00..00, max=0xFF..FF:
        //   rMax = 0xFF..FF, flip bit 0 -> 0x7F..FF
        //   rMin = 0x00..00 (unchanged)
        //   min_ = 0x00..00, flip bit 0 -> 0x80..00
        //   max_ = 0xFF..FF (unchanged)
        //
        //   Self (left/upper): [0x80..00, 0xFF..FF] prefix=1
        //   rBucket (right/lower): [0x00..00, 0x7F..FF] prefix=1

        let mut r_max = self.max_id;
        let r_min = self.min_id;

        r_max.flip_bit(self.prefix_length);
        self.min_id.flip_bit(self.prefix_length);
        self.prefix_length += 1;

        let mut r_bucket =
            DhtBucket::with_range(self.prefix_length, r_max, r_min, self.local_node_id);

        // Redistribute nodes between the two halves
        let mut remaining = VecDeque::with_capacity(K);
        for node in self.nodes.drain(..) {
            if r_bucket.is_in_range(node.id()) {
                r_bucket.nodes.push_back(node);
            } else {
                remaining.push_back(node);
            }
        }
        self.nodes = remaining;

        trace!(
            left_prefix = self.prefix_length,
            left_min = %self.min_id,
            left_max = %self.max_id,
            left_nodes = self.nodes.len(),
            right_prefix = r_bucket.prefix_length,
            right_nodes = r_bucket.nodes.len(),
            "Bucket split completed"
        );

        r_bucket
    }

    /// Get references to the good (non-bad) nodes in this bucket.
    ///
    /// C++: `getGoodNodes()` — returns all nodes that are not bad.
    pub fn good_nodes(&self) -> impl Iterator<Item = &DhtNode> {
        self.nodes
            .iter()
            .filter(|n| !n.is_bad())
            .map(|b| b.as_ref())
    }

    /// Return the number of nodes in this bucket.
    pub fn count(&self) -> usize {
        self.nodes.len()
    }

    /// Check if the bucket contains a node with the given ID.
    pub fn contains(&self, node_id: &NodeId) -> bool {
        self.nodes.iter().any(|n| n.id() == node_id)
    }

    /// Check if this bucket needs a refresh.
    ///
    /// C++: `needsRefresh()` — returns true if bucket has fewer than K nodes
    /// or hasn't been updated in BUCKET_REFRESH_INTERVAL.
    pub fn needs_refresh(&self) -> bool {
        use super::constants::BUCKET_REFRESH_INTERVAL_SECS;
        self.nodes.len() < K
            || self.last_updated.elapsed().as_secs() >= BUCKET_REFRESH_INTERVAL_SECS
    }

    /// Check if this bucket contains any questionable node.
    ///
    /// C++: `containsQuestionableNode()`
    pub fn contains_questionable_node(&self) -> bool {
        self.nodes.iter().any(|n| n.is_questionable())
    }

    /// Get the least-recently-used questionable node.
    ///
    /// C++: `getLRUQuestionableNode()` — returns the first (LRU) questionable node.
    pub fn lru_questionable_node(&self) -> Option<&DhtNode> {
        self.nodes
            .iter()
            .find(|n| n.is_questionable())
            .map(|b| b.as_ref())
    }

    /// Update the last-updated timestamp.
    ///
    /// C++: `notifyUpdate()`
    pub fn notify_update(&mut self) {
        self.last_updated = Instant::now();
    }

    /// Return the prefix length.
    pub fn prefix_length(&self) -> usize {
        self.prefix_length
    }

    /// Alias for `prefix_length` — matches C++ `DHTBucket::getPrefixLength`.
    pub fn common_prefix_len(&self) -> usize {
        self.prefix_length
    }

    /// Time elapsed since the last update to this bucket.
    pub fn time_since_last_update(&self) -> std::time::Duration {
        self.last_updated.elapsed()
    }

    /// Generate a random node ID that falls within this bucket's range.
    ///
    /// Used by bucket refresh to create a lookup target inside the bucket.
    /// The ID shares the first `prefix_length` bits with `min_id`, and the
    /// remaining bits are random.
    pub fn random_id_in_range(&self) -> NodeId {
        let mut id = *self.min_id.as_bytes();
        // The first prefix_length bits are fixed; randomize the rest
        for i in self.prefix_length / 8..ID_LENGTH {
            id[i] = rand::random::<u8>();
        }
        // Randomize the remaining bits in the boundary byte
        if self.prefix_length % 8 != 0 {
            let byte_idx = self.prefix_length / 8;
            let mask = 0xFFu8 >> (self.prefix_length % 8);
            id[byte_idx] = (id[byte_idx] & !mask) | (rand::random::<u8>() & mask);
        }
        NodeId(id)
    }

    /// Return the minimum ID of this bucket's range.
    pub fn min_id(&self) -> &NodeId {
        &self.min_id
    }

    /// Return the maximum ID of this bucket's range.
    pub fn max_id(&self) -> &NodeId {
        &self.max_id
    }

    /// Return a reference to the nodes in this bucket.
    pub fn nodes(&self) -> &VecDeque<Box<DhtNode>> {
        &self.nodes
    }

    /// Return a reference to the cached replacement nodes.
    pub fn cached_nodes(&self) -> &VecDeque<Box<DhtNode>> {
        &self.cached_nodes
    }
}

impl std::fmt::Debug for DhtBucket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DhtBucket")
            .field("prefix_length", &self.prefix_length)
            .field("min_id", &self.min_id)
            .field("max_id", &self.max_id)
            .field("node_count", &self.nodes.len())
            .field("cache_count", &self.cached_nodes.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
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
    fn new_bucket_covers_full_range() {
        let bucket = DhtBucket::new(NodeId::ZERO);
        assert_eq!(bucket.prefix_length(), 0);
        assert_eq!(bucket.min_id(), &NodeId::ZERO);
        assert_eq!(bucket.max_id(), &NodeId::MAX);
        assert_eq!(bucket.count(), 0);
    }

    #[test]
    fn add_node_within_capacity() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        for i in 0..K {
            let node = make_node(i as u8);
            assert!(bucket.add_node(node));
        }
        assert_eq!(bucket.count(), K);
    }

    #[test]
    fn add_node_when_full_returns_false() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        for i in 0..K {
            let mut node = make_node(i as u8);
            node.mark_good();
            node.update_last_contact();
            bucket.add_node(node);
        }
        // All nodes are good, bucket is full
        let extra = make_node(0xFF);
        assert!(!bucket.add_node(extra));
    }

    #[test]
    fn add_node_evicts_bad_node_when_full() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        for i in 0..K {
            let mut node = make_node(i as u8);
            if i == 0 {
                node.mark_bad();
            }
            bucket.add_node(node);
        }
        assert_eq!(bucket.count(), K);

        let new_node = make_node(0xFF);
        assert!(bucket.add_node(new_node));
        assert_eq!(bucket.count(), K);
        assert!(!bucket.contains(&NodeId::from_slice(&[0u8; ID_LENGTH])));
        assert!(bucket.contains(&NodeId::from_slice(&[0xFFu8; ID_LENGTH])));
    }

    #[test]
    fn add_existing_node_moves_to_tail() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        let mut node = make_node(0x42);
        node.mark_good();
        node.update_last_contact();
        bucket.add_node(node);

        let mut node2 = make_node(0x43);
        node2.mark_good();
        bucket.add_node(node2);

        // Re-add node 0x42 — it should move to tail (MRU)
        let node3 = make_node(0x42);
        bucket.add_node(node3);
        assert_eq!(bucket.count(), 2);
        assert_eq!(
            bucket.nodes().back().unwrap().id(),
            &NodeId::from_slice(&[0x42u8; ID_LENGTH])
        );
    }

    #[test]
    fn cache_node_respects_cache_size() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        for i in 0..CACHE_SIZE + 2 {
            let node = make_node(i as u8);
            bucket.cache_node(node);
        }
        assert!(bucket.cached_nodes().len() <= CACHE_SIZE);
    }

    #[test]
    fn drop_node_replaces_from_cache() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        let node = make_node(0x01);
        bucket.add_node(node);

        let cached = make_node(0x02);
        bucket.cache_node(cached);

        bucket.drop_node(&NodeId::from_slice(&[0x01u8; ID_LENGTH]));
        assert!(!bucket.contains(&NodeId::from_slice(&[0x01u8; ID_LENGTH])));
        assert!(bucket.contains(&NodeId::from_slice(&[0x02u8; ID_LENGTH])));
    }

    #[test]
    fn split_produces_two_halves() {
        let mut bucket = DhtBucket::new(NodeId::from_slice(&[0x80u8; ID_LENGTH]));

        // Add nodes to both halves
        let upper_node = make_node(0xFF); // belongs to upper half
        let lower_node = make_node(0x01); // belongs to lower half
        bucket.add_node(upper_node);
        bucket.add_node(lower_node);

        let r_bucket = bucket.split();

        // Self (left/upper): [0x80..00, 0xFF..FF]
        assert_eq!(bucket.prefix_length(), 1);
        assert!(bucket.min_id().get_bit(0)); // bit 0 set = upper half
        assert_eq!(bucket.count(), 1);
        assert!(bucket.contains(&NodeId::from_slice(&[0xFFu8; ID_LENGTH])));

        // r_bucket (right/lower): [0x00..00, 0x7F..FF]
        assert_eq!(r_bucket.prefix_length(), 1);
        assert!(!r_bucket.min_id().get_bit(0)); // bit 0 not set = lower half
        assert_eq!(r_bucket.count(), 1);
        assert!(r_bucket.contains(&NodeId::from_slice(&[0x01u8; ID_LENGTH])));
    }

    #[test]
    fn split_allowed_only_when_local_in_range() {
        let bucket = DhtBucket::new(NodeId::from_slice(&[0x80u8; ID_LENGTH]));
        assert!(bucket.split_allowed());
    }

    #[test]
    fn split_not_allowed_at_max_prefix() {
        let bucket = DhtBucket::with_range(
            ID_LENGTH * 8 - 1,
            NodeId::MAX,
            NodeId::ZERO,
            NodeId::from_slice(&[0x80u8; ID_LENGTH]),
        );
        assert!(!bucket.split_allowed());
    }

    #[test]
    fn is_in_range() {
        let bucket = DhtBucket::new(NodeId::ZERO);
        assert!(bucket.is_in_range(&NodeId::ZERO));
        assert!(bucket.is_in_range(&NodeId::MAX));
        assert!(bucket.is_in_range(&NodeId::from_slice(&[0x80u8; ID_LENGTH])));
    }

    #[test]
    fn needs_refresh_when_under_full() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        assert!(bucket.needs_refresh());
        let node = make_node(0x01);
        bucket.add_node(node);
        assert!(bucket.needs_refresh());
    }

    #[test]
    fn contains_questionable_node() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        let mut node = make_node(0x01);
        node.mark_good();
        // No last_contact -> questionable
        assert!(node.is_questionable());
        bucket.add_node(node);
        assert!(bucket.contains_questionable_node());
    }

    #[test]
    fn move_to_head_and_tail() {
        let mut bucket = DhtBucket::new(NodeId::ZERO);
        let n1 = make_node(0x01);
        let n2 = make_node(0x02);
        bucket.add_node(n1);
        bucket.add_node(n2);

        // n2 is at tail (MRU), n1 at head (LRU)
        assert_eq!(
            bucket.nodes().front().unwrap().id(),
            &NodeId::from_slice(&[0x01u8; ID_LENGTH])
        );

        // Move n2 to head
        bucket.move_to_head(&NodeId::from_slice(&[0x02u8; ID_LENGTH]));
        assert_eq!(
            bucket.nodes().front().unwrap().id(),
            &NodeId::from_slice(&[0x02u8; ID_LENGTH])
        );

        // Move n2 to tail
        bucket.move_to_tail(&NodeId::from_slice(&[0x02u8; ID_LENGTH]));
        assert_eq!(
            bucket.nodes().back().unwrap().id(),
            &NodeId::from_slice(&[0x02u8; ID_LENGTH])
        );
    }
}
