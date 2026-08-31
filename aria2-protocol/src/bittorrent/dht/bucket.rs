//! DHT K-bucket — holds up to K nodes for a specific ID range.
//!
//! Each bucket covers a contiguous range of DHT node IDs [min_id, max_id]
//! (inclusive) and holds up to `K` nodes that fall within that range. When
//! the bucket is full and contains the local node's ID, it can be split
//! into two halves.
//!
//! A replacement cache (`CACHE_SIZE = 2`) stores candidate nodes that can
//! replace questionable nodes in the main bucket. This is the Rust
//! equivalent of C++ `DHTBucket`.

use std::time::Instant;

use super::node::DhtNode;

/// K-bucket constant: maximum nodes per bucket (BEP 5).
pub const K: usize = 8;

/// Maximum replacement cache entries per bucket.
pub const CACHE_SIZE: usize = 2;

/// DHT ID length in bytes.
const ID_LENGTH: usize = 20;

/// Bucket refresh interval in seconds (15 minutes, matching C++ DHT_BUCKET_REFRESH_INTERVAL).
const BUCKET_REFRESH_INTERVAL_SECS: u64 = 15 * 60;

// ---------------------------------------------------------------------------
// Bit manipulation helpers
// ---------------------------------------------------------------------------

/// Flip the bit at `bit_index` (0 = MSB of byte 0, 159 = LSB of byte 19)
/// in the given ID buffer.
fn flip_bit(id: &mut [u8; 20], bit_index: usize) {
    let byte_index = bit_index / 8;
    let bit_offset = 7 - (bit_index % 8);
    if byte_index < 20 {
        id[byte_index] ^= 1 << bit_offset;
    }
}

// ---------------------------------------------------------------------------
// Bucket
// ---------------------------------------------------------------------------

/// A Kademlia DHT k-bucket holding nodes for a specific ID range.
///
/// Each bucket covers IDs in the range `[min_id, max_id]` (inclusive) and
/// can hold up to `K` nodes. When the bucket is full, new nodes are either
/// rejected or replace bad/questionable nodes.
///
/// Buckets also maintain a replacement cache of up to `CACHE_SIZE` nodes
/// that can be promoted when existing nodes become unresponsive.
#[derive(Debug)]
pub struct Bucket {
    /// Prefix length for this bucket (number of leading bits that are fixed).
    prefix_length: usize,

    /// Minimum ID in this bucket's range (inclusive).
    min_id: [u8; 20],

    /// Maximum ID in this bucket's range (inclusive).
    max_id: [u8; 20],

    /// The local node's ID (used to determine if splitting is allowed).
    local_id: [u8; 20],

    /// Nodes in this bucket, sorted by last-seen time (LRU at front).
    nodes: Vec<DhtNode>,

    /// Replacement cache, sorted by last-seen time (most recent at front).
    cached_nodes: Vec<DhtNode>,

    /// Time of last update to this bucket.
    last_updated: Instant,
}

impl Bucket {
    /// Create a new bucket covering the full ID space [0x00.., 0xFF..].
    ///
    /// This is the initial bucket that will be split as the routing table
    /// grows. Equivalent to C++ `DHTBucket(localNode)`.
    pub fn new(local_node: &DhtNode) -> Self {
        Self {
            prefix_length: 0,
            min_id: [0u8; 20],
            max_id: [0xFFu8; 20],
            local_id: local_node.id,
            nodes: Vec::with_capacity(K),
            cached_nodes: Vec::with_capacity(CACHE_SIZE),
            last_updated: Instant::now(),
        }
    }

    /// Create a bucket with a specific ID range and prefix length.
    ///
    /// Equivalent to C++ `DHTBucket(prefixLength, max, min, localNode)`.
    pub fn new_for_range(
        prefix_length: usize,
        min_id: [u8; 20],
        max_id: [u8; 20],
        local_id: [u8; 20],
    ) -> Self {
        Self {
            prefix_length,
            min_id,
            max_id,
            local_id,
            nodes: Vec::with_capacity(K),
            cached_nodes: Vec::with_capacity(CACHE_SIZE),
            last_updated: Instant::now(),
        }
    }

    // -----------------------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------------------

    /// Returns the prefix length (number of fixed leading bits).
    pub fn prefix_length(&self) -> usize {
        self.prefix_length
    }

    /// Returns the minimum ID in this bucket's range.
    pub fn min_id(&self) -> &[u8; 20] {
        &self.min_id
    }

    /// Returns the maximum ID in this bucket's range.
    pub fn max_id(&self) -> &[u8; 20] {
        &self.max_id
    }

    /// Returns the number of nodes in this bucket.
    pub fn count_node(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the bucket has reached its maximum capacity (K).
    pub fn is_full(&self) -> bool {
        self.nodes.len() >= K
    }

    /// Returns `true` if the bucket has no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns a reference to the nodes in this bucket.
    pub fn nodes(&self) -> &[DhtNode] {
        &self.nodes
    }

    /// Returns a reference to the replacement cache.
    pub fn cached_nodes(&self) -> &[DhtNode] {
        &self.cached_nodes
    }

    /// Returns the number of good (non-bad) nodes.
    pub fn good_node_count(&self) -> usize {
        self.nodes.iter().filter(|n| !n.is_bad()).count()
    }

    // -----------------------------------------------------------------------
    // Range checks
    // -----------------------------------------------------------------------

    /// Returns `true` if `node_id` falls within this bucket's [min, max] range.
    ///
    /// Uses lexicographic comparison, equivalent to C++ `isInRange()`.
    pub fn is_in_range(&self, node_id: &[u8; 20]) -> bool {
        node_id.as_slice() >= self.min_id.as_slice() && node_id.as_slice() <= self.max_id.as_slice()
    }

    // -----------------------------------------------------------------------
    // Node management
    // -----------------------------------------------------------------------

    /// Try to add a node to this bucket.
    ///
    /// - If the node's ID already exists, update it (move to tail).
    /// - If there's room, add the node.
    /// - If the bucket is full and the LRU node is bad, replace it.
    /// - Otherwise, return `false` (bucket is full of good/questionable nodes).
    ///
    /// Equivalent to C++ `DHTBucket::addNode()`.
    pub fn add_node(&mut self, node: DhtNode) -> bool {
        self.notify_update();

        // Check if the node already exists.
        if let Some(pos) = self.nodes.iter().position(|n| n.id == node.id) {
            // Update existing: remove old and add to tail (most recently seen).
            self.nodes.remove(pos);
            self.nodes.push(node);
            return true;
        }

        if self.nodes.len() < K {
            self.nodes.push(node);
            return true;
        }

        // Bucket is full. Try to replace a bad node (LRU = front).
        if let Some(front) = self.nodes.first()
            && front.is_bad()
        {
            self.nodes.remove(0);
            self.nodes.push(node);
            return true;
        }

        false
    }

    /// Cache a node for potential future replacement.
    ///
    /// Cached nodes are sorted by last-seen time (most recent at front).
    /// The cache is limited to `CACHE_SIZE` entries.
    ///
    /// Equivalent to C++ `DHTBucket::cacheNode()`.
    pub fn cache_node(&mut self, node: DhtNode) {
        self.cached_nodes.insert(0, node);
        if self.cached_nodes.len() > CACHE_SIZE {
            self.cached_nodes.truncate(CACHE_SIZE);
        }
    }

    /// Drop a node from the bucket, promoting the first cached node if available.
    ///
    /// Equivalent to C++ `DHTBucket::dropNode()`.
    /// Returns `true` if a node was removed.
    pub fn drop_node(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == node_id) {
            self.nodes.remove(pos);
            // Promote the first cached node if available.
            if !self.cached_nodes.is_empty() {
                let replacement = self.cached_nodes.remove(0);
                self.nodes.push(replacement);
            }
            true
        } else {
            false
        }
    }

    /// Remove a node from the bucket (no replacement promotion).
    pub fn remove_node(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == node_id) {
            self.nodes.remove(pos);
            true
        } else {
            false
        }
    }

    /// Replace a specific node with a candidate from this bucket.
    pub fn replace_node(&mut self, node_id: &[u8; 20], replacement: DhtNode) -> bool {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == node_id) {
            self.nodes.remove(pos);
            self.nodes.push(replacement);
            true
        } else {
            false
        }
    }

    /// Remove a replacement candidate by ID.
    pub fn remove_cached_node(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(pos) = self
            .cached_nodes
            .iter()
            .position(|node| &node.id == node_id)
        {
            self.cached_nodes.remove(pos);
            true
        } else {
            false
        }
    }

    /// Move a node to the head (front / LRU position) of the bucket.
    ///
    /// Equivalent to C++ `DHTBucket::moveToHead()`.
    pub fn move_to_head(&mut self, node_id: &[u8; 20]) {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == node_id) {
            let node = self.nodes.remove(pos);
            self.nodes.insert(0, node);
        }
    }

    /// Move a node to the tail (MRU position) of the bucket.
    ///
    /// Equivalent to C++ `DHTBucket::moveToTail()`.
    pub fn move_to_tail(&mut self, node_id: &[u8; 20]) {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == node_id) {
            let node = self.nodes.remove(pos);
            self.nodes.push(node);
        }
    }

    // -----------------------------------------------------------------------
    // Splitting
    // -----------------------------------------------------------------------

    /// Returns `true` if this bucket is allowed to be split.
    ///
    /// A bucket can be split if:
    /// 1. Its prefix length is less than 159 (cannot split beyond max depth).
    /// 2. The local node's ID falls within this bucket's range.
    ///
    /// Equivalent to C++ `DHTBucket::splitAllowed()`.
    pub fn split_allowed(&self) -> bool {
        self.prefix_length < ID_LENGTH * 8 - 1 && self.is_in_range(&self.local_id)
    }

    /// Split this bucket into two halves.
    ///
    /// This method mutates `self` to become the left half and returns the new
    /// right bucket. The split is done by flipping the bit at `prefix_length`:
    /// - Left half: IDs where the bit at `prefix_length` matches `min_id`
    /// - Right half: IDs where the bit at `prefix_length` is flipped
    ///
    /// After splitting, both buckets have `prefix_length + 1`.
    /// Nodes are redistributed to the appropriate half.
    ///
    /// Equivalent to C++ `DHTBucket::split()`.
    pub fn split(&mut self) -> Bucket {
        assert!(
            self.split_allowed(),
            "split() called on non-splittable bucket"
        );

        // Right bucket's max = current max.
        let r_max = self.max_id;

        // Right bucket's min = current min with bit at prefix_length flipped.
        let mut r_min = self.min_id;
        flip_bit(&mut r_min, self.prefix_length);

        // Left bucket's max = current max with bit at prefix_length flipped.
        flip_bit(&mut self.max_id, self.prefix_length);

        // Increment prefix length.
        self.prefix_length += 1;
        let new_prefix_length = self.prefix_length;

        // Create the right bucket.
        let mut right_bucket =
            Bucket::new_for_range(new_prefix_length, r_min, r_max, self.local_id);

        // Redistribute nodes.
        let mut remaining = Vec::with_capacity(K);
        for node in self.nodes.drain(..) {
            if right_bucket.is_in_range(&node.id) {
                right_bucket.nodes.push(node);
            } else {
                remaining.push(node);
            }
        }
        self.nodes = remaining;

        tracing::debug!(
            "Bucket split: left prefix={} range={}-{}, right prefix={} range={}-{}",
            self.prefix_length,
            hex::encode(self.min_id),
            hex::encode(self.max_id),
            right_bucket.prefix_length,
            hex::encode(right_bucket.min_id),
            hex::encode(right_bucket.max_id),
        );

        right_bucket
    }

    // -----------------------------------------------------------------------
    // Query helpers
    // -----------------------------------------------------------------------

    /// Returns `true` if this bucket needs a refresh.
    ///
    /// A bucket needs refresh if it has fewer than K nodes or hasn't been
    /// updated in `BUCKET_REFRESH_INTERVAL`.
    pub fn needs_refresh(&self) -> bool {
        self.nodes.len() < K
            || self.last_updated.elapsed().as_secs() >= BUCKET_REFRESH_INTERVAL_SECS
    }

    /// Returns `true` if this bucket contains at least one questionable node.
    pub fn contains_questionable_node(&self) -> bool {
        self.nodes.iter().any(|n| n.is_questionable())
    }

    /// Returns the LRU (least recently used) questionable node, if any.
    ///
    /// Equivalent to C++ `DHTBucket::getLRUQuestionableNode()`.
    pub fn get_lru_questionable_node(&self) -> Option<&DhtNode> {
        self.nodes.iter().find(|n| n.is_questionable())
    }

    /// Get a specific node by ID, IP address, and port.
    ///
    /// Equivalent to C++ `DHTBucket::getNode()`.
    pub fn get_node(&self, node_id: &[u8; 20], ip_addr: &str, port: u16) -> Option<&DhtNode> {
        self.nodes.iter().find(|n| {
            &n.id == node_id && n.addr.ip().to_string() == ip_addr && n.addr.port() == port
        })
    }

    /// Generate a random node ID that falls within this bucket's range.
    ///
    /// Equivalent to C++ `DHTBucket::getRandomNodeID()`.
    pub fn get_random_node_id(&self) -> [u8; 20] {
        use rand::Rng;
        let mut id = [0u8; 20];
        let mut rng = rand::thread_rng();

        if self.prefix_length == 0 {
            // Full range — generate completely random.
            rng.fill(&mut id);
        } else {
            // Copy the fixed prefix from min_id, randomize the rest.
            let last_byte_index = (self.prefix_length - 1) / 8;
            rng.fill(&mut id);
            // Overwrite the prefix portion.
            id[..=last_byte_index].copy_from_slice(&self.min_id[..=last_byte_index]);
        }

        id
    }

    /// Mark a node as good (reset failure count and update last_seen).
    pub fn mark_good(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(node) = self.nodes.iter_mut().find(|n| &n.id == node_id) {
            node.touch();
            true
        } else {
            false
        }
    }

    /// Mark a node as bad (increment failure count).
    pub fn mark_bad(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(node) = self.nodes.iter_mut().find(|n| &n.id == node_id) {
            node.record_failure();
            true
        } else {
            false
        }
    }

    /// Evict all bad nodes from this bucket.
    ///
    /// Returns the number of nodes evicted.
    pub fn evict_bad(&mut self) -> usize {
        let before = self.nodes.len();
        self.nodes.retain(|n| !n.is_bad());
        before - self.nodes.len()
    }

    /// Update the last-updated timestamp.
    fn notify_update(&mut self) {
        self.last_updated = Instant::now();
    }

    /// Collect good nodes (non-bad) from this bucket.
    pub fn get_good_nodes(&self) -> Vec<DhtNode> {
        self.nodes.iter().filter(|n| !n.is_bad()).cloned().collect()
    }

    /// Count questionable nodes in this bucket.
    pub fn questionable_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_questionable()).count()
    }

    /// Count bad nodes in this bucket.
    pub fn bad_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_bad()).count()
    }
}

impl Clone for Bucket {
    fn clone(&self) -> Self {
        Self {
            prefix_length: self.prefix_length,
            min_id: self.min_id,
            max_id: self.max_id,
            local_id: self.local_id,
            nodes: self.nodes.clone(),
            cached_nodes: self.cached_nodes.clone(),
            last_updated: self.last_updated,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn make_local_node() -> DhtNode {
        DhtNode::new([0u8; 20], "127.0.0.1:6881".parse::<SocketAddr>().unwrap())
    }

    #[test]
    fn test_bucket_creation_full_range() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        assert_eq!(bucket.prefix_length(), 0);
        assert_eq!(bucket.min_id(), &[0u8; 20]);
        assert_eq!(bucket.max_id(), &[0xFFu8; 20]);
        assert_eq!(bucket.count_node(), 0);
        assert!(!bucket.is_full());
    }

    #[test]
    fn test_bucket_is_in_range() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        assert!(bucket.is_in_range(&[0u8; 20]));
        assert!(bucket.is_in_range(&[0xFFu8; 20]));
        assert!(bucket.is_in_range(&[0x80u8; 20]));
    }

    #[test]
    fn test_add_node() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);
        let node = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
        assert!(bucket.add_node(node));
        assert_eq!(bucket.count_node(), 1);
    }

    #[test]
    fn test_add_node_full_bucket_replaces_bad() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);

        // Fill the bucket with bad nodes.
        for i in 0..K {
            let mut node = DhtNode::new(
                [i as u8; 20],
                format!("127.0.0.1:{}", 6882 + i).parse().unwrap(),
            );
            for _ in 0..3 {
                node.record_failure();
            }
            bucket.add_node(node);
        }
        assert!(bucket.is_full());

        // The first node (id=[0]) should be bad and LRU.
        let new_node = DhtNode::new([0xFFu8; 20], "127.0.0.1:9999".parse().unwrap());
        assert!(bucket.add_node(new_node));
        assert_eq!(bucket.count_node(), K);
    }

    #[test]
    fn test_add_node_full_bucket_rejects() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);

        // Fill with good nodes.
        for i in 0..K {
            let node = DhtNode::new(
                [(i + 1) as u8; 20],
                format!("127.0.0.1:{}", 6882 + i).parse().unwrap(),
            );
            bucket.add_node(node);
        }
        assert!(bucket.is_full());

        // All nodes are good; new node should be rejected.
        let new_node = DhtNode::new([0xFFu8; 20], "127.0.0.1:9999".parse().unwrap());
        assert!(!bucket.add_node(new_node));
    }

    #[test]
    fn test_split_allowed() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        // Full-range bucket containing local node ID — should be splittable.
        assert!(bucket.split_allowed());
    }

    #[test]
    fn test_split_not_allowed_if_local_id_out_of_range() {
        let local = DhtNode::new([0xFFu8; 20], "127.0.0.1:6881".parse().unwrap());
        let bucket = Bucket::new_for_range(
            1,
            [0u8; 20],    // min
            [0x7Fu8; 20], // max (first bit = 0)
            local.id,
        );
        // Local ID [0xFF..] is not in range [0x00.., 0x7F..]
        assert!(!bucket.split_allowed());
    }

    #[test]
    fn test_split() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);

        // Add nodes to the left half (first bit = 0).
        for i in 0..4u8 {
            let node = DhtNode::new(
                [i; 20],
                format!("127.0.0.1:{}", 6882 + i as u16).parse().unwrap(),
            );
            bucket.add_node(node);
        }

        // Add nodes to the right half (first bit = 1).
        for i in 0..4u8 {
            let mut id = [0u8; 20];
            id[0] = 0x80 | i;
            let node = DhtNode::new(
                id,
                format!("127.0.0.2:{}", 6882 + i as u16).parse().unwrap(),
            );
            bucket.add_node(node);
        }

        assert_eq!(bucket.count_node(), 8);

        // Split the bucket.
        let right_bucket = bucket.split();

        // Left bucket should have nodes with first bit = 0.
        assert_eq!(bucket.prefix_length(), 1);
        assert_eq!(bucket.count_node(), 4);

        // Right bucket should have nodes with first bit = 1.
        assert_eq!(right_bucket.prefix_length(), 1);
        assert_eq!(right_bucket.count_node(), 4);
    }

    #[test]
    fn test_cache_node() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);
        let node = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
        bucket.cache_node(node);
        assert_eq!(bucket.cached_nodes().len(), 1);
    }

    #[test]
    fn test_cache_node_limit() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);
        for i in 0..5u8 {
            let node = DhtNode::new(
                [i; 20],
                format!("127.0.0.1:{}", 6882 + i as u16).parse().unwrap(),
            );
            bucket.cache_node(node);
        }
        assert_eq!(bucket.cached_nodes().len(), CACHE_SIZE);
    }

    #[test]
    fn test_drop_node_promotes_cached() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);

        // Add a node.
        let node_id = [1u8; 20];
        let node = DhtNode::new(node_id, "127.0.0.1:6882".parse().unwrap());
        bucket.add_node(node);

        // Cache a replacement.
        let replacement = DhtNode::new([2u8; 20], "127.0.0.1:6883".parse().unwrap());
        bucket.cache_node(replacement);

        // Drop the original node.
        assert!(bucket.drop_node(&node_id));
        assert_eq!(bucket.count_node(), 1);
        // The replacement should now be in the main node list.
        assert!(bucket.nodes().iter().any(|n| n.id == [2u8; 20]));
    }

    #[test]
    fn test_replace_node_consumes_cached_candidate() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);
        let node_id = [1u8; 20];
        let replacement_id = [2u8; 20];

        bucket.add_node(DhtNode::new(node_id, "127.0.0.1:6882".parse().unwrap()));
        bucket.cache_node(DhtNode::new(
            replacement_id,
            "127.0.0.1:6883".parse().unwrap(),
        ));

        assert!(bucket.remove_cached_node(&replacement_id));
        assert!(bucket.replace_node(
            &node_id,
            DhtNode::new(replacement_id, "127.0.0.1:6883".parse().unwrap())
        ));
        assert!(!bucket.nodes().iter().any(|node| node.id == node_id));
        assert!(bucket.nodes().iter().any(|node| node.id == replacement_id));
        assert!(bucket.cached_nodes().is_empty());
    }

    #[test]
    fn test_get_random_node_id() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        let id = bucket.get_random_node_id();
        // For a full-range bucket, the ID should be within range.
        assert!(bucket.is_in_range(&id));
    }

    #[test]
    fn test_needs_refresh_empty() {
        let local = make_local_node();
        let bucket = Bucket::new(&local);
        // Empty bucket needs refresh.
        assert!(bucket.needs_refresh());
    }

    #[test]
    fn test_contains_questionable_node() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);

        // Fresh node is not questionable.
        let node = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
        bucket.add_node(node);
        assert!(!bucket.contains_questionable_node());
    }

    #[test]
    fn test_move_to_tail() {
        let local = make_local_node();
        let mut bucket = Bucket::new(&local);

        let n1 = DhtNode::new([1u8; 20], "127.0.0.1:6882".parse().unwrap());
        let n2 = DhtNode::new([2u8; 20], "127.0.0.1:6883".parse().unwrap());
        bucket.add_node(n1);
        bucket.add_node(n2);

        // Move n1 to tail.
        bucket.move_to_tail(&[1u8; 20]);
        assert_eq!(bucket.nodes()[1].id, [1u8; 20]);
    }
}
