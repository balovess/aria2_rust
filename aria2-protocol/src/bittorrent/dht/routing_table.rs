use super::bucket::Bucket;
use super::node::DhtNode;

const BUCKET_COUNT: usize = 160;

pub struct RoutingTable {
    buckets: Vec<Bucket>,
    self_id: [u8; 20],
}

impl RoutingTable {
    pub fn new(self_id: [u8; 20]) -> Self {
        Self {
            buckets: (0..BUCKET_COUNT).map(|_| Bucket::new()).collect(),
            self_id,
        }
    }

    pub fn insert(&mut self, node: DhtNode) {
        let bucket_idx = self.bucket_index_for(&node.id);
        if bucket_idx >= BUCKET_COUNT {
            return;
        }

        if let Some(evicted) = self.buckets[bucket_idx].insert(node) {
            tracing::debug!("DHT node evicted: {}", evicted.id_hex());
        }
    }

    pub fn remove(&mut self, node_id: &[u8; 20]) -> bool {
        let idx = self.bucket_index_for(node_id);
        if idx >= BUCKET_COUNT {
            return false;
        }
        self.buckets[idx].remove(node_id)
    }

    pub fn find_closest(&self, target: &[u8; 20], count: usize) -> Vec<&DhtNode> {
        let mut all_nodes: Vec<(usize, &DhtNode)> = self
            .buckets
            .iter()
            .enumerate()
            .flat_map(|(i, b)| b.get_nodes().iter().map(move |n| (i, n)))
            .collect();

        all_nodes.sort_by_key(|(_, n)| n.distance_to(target));

        all_nodes.into_iter().take(count).map(|(_, n)| n).collect()
    }

    pub fn get_bucket(&self, index: usize) -> Option<&Bucket> {
        self.buckets.get(index)
    }

    pub fn total_node_count(&self) -> usize {
        self.buckets.iter().map(|b| b.len()).sum()
    }

    pub fn good_node_count(&self) -> usize {
        self.buckets.iter().map(|b| b.get_good_count()).sum()
    }

    pub fn evict_bad_nodes(&mut self) -> usize {
        self.buckets.iter_mut().map(|b| b.evict_bad()).sum()
    }

    /// Mark a node as good (reset failure count and update last_seen)
    pub fn mark_good(&mut self, node_id: &[u8; 20]) -> bool {
        let idx = self.bucket_index_for(node_id);
        if idx >= BUCKET_COUNT {
            return false;
        }
        self.buckets[idx].mark_good(node_id)
    }

    /// Mark a node as bad (increment failure count)
    pub fn mark_bad(&mut self, node_id: &[u8; 20]) -> bool {
        let idx = self.bucket_index_for(node_id);
        if idx >= BUCKET_COUNT {
            return false;
        }
        self.buckets[idx].mark_bad(node_id)
    }

    /// Mark a node as questionable (set last_seen to old timestamp)
    pub fn mark_questionable(&mut self, node_id: &[u8; 20]) -> bool {
        let idx = self.bucket_index_for(node_id);
        if idx >= BUCKET_COUNT {
            return false;
        }
        self.buckets[idx].mark_questionable(node_id)
    }

    /// Get a random node from the routing table for bucket refresh
    pub fn get_random_node(&self) -> Option<&DhtNode> {
        use rand::Rng;
        use rand::seq::SliceRandom;
        let mut rng = rand::thread_rng();

        // Collect all non-empty buckets
        let non_empty_buckets: Vec<(usize, &Bucket)> = self
            .buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.is_empty())
            .collect();

        if non_empty_buckets.is_empty() {
            return None;
        }

        // Pick a random bucket
        let (_, bucket) = non_empty_buckets.choose(&mut rng)?;
        let nodes = bucket.get_nodes();

        if nodes.is_empty() {
            return None;
        }

        // Pick a random node from the bucket
        let idx = rng.gen_range(0..nodes.len());
        Some(&nodes[idx])
    }

    /// Get all buckets that need refresh (haven't been updated in 15 minutes)
    pub fn get_buckets_needing_refresh(&self) -> Vec<usize> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| b.needs_refresh())
            .map(|(i, _)| i)
            .collect()
    }

    /// Count questionable nodes in the routing table
    pub fn questionable_node_count(&self) -> usize {
        self.buckets
            .iter()
            .map(|b| b.get_questionable_count())
            .sum()
    }

    /// Count bad nodes in the routing table
    pub fn bad_node_count(&self) -> usize {
        self.buckets.iter().map(|b| b.get_bad_count()).sum()
    }

    /// Refresh buckets that haven't been updated in 15 minutes
    /// Returns a list of target IDs to query for each bucket needing refresh
    pub fn refresh_buckets(&self) -> Vec<[u8; 20]> {
        let mut targets = Vec::new();

        for (idx, bucket) in self.buckets.iter().enumerate() {
            if bucket.needs_refresh() {
                // Generate a random ID in this bucket's range
                let target = self.generate_random_id_in_bucket(idx);
                targets.push(target);
            }
        }

        targets
    }

    /// Get all questionable nodes (nodes not seen in 15 minutes)
    pub fn get_questionable_nodes(&self) -> Vec<&DhtNode> {
        let mut nodes = Vec::new();

        for bucket in &self.buckets {
            for node in bucket.get_nodes() {
                if node.is_questionable() {
                    nodes.push(node);
                }
            }
        }

        nodes
    }

    /// Fill the routing table by finding nodes close to our own ID
    /// Returns a list of target IDs to query
    pub fn fill_routing_table(&self) -> Vec<[u8; 20]> {
        let mut targets = Vec::new();

        // Find buckets that are not full
        for (idx, bucket) in self.buckets.iter().enumerate() {
            if !bucket.is_full() {
                // Generate a random ID in this bucket's range
                let target = self.generate_random_id_in_bucket(idx);
                targets.push(target);
            }
        }

        targets
    }

    /// Generate a random ID that falls within a specific bucket's range
    fn generate_random_id_in_bucket(&self, bucket_idx: usize) -> [u8; 20] {
        use rand::Rng;
        let mut rng = rand::thread_rng();

        let mut id = self.self_id;

        // Set the bits to place the ID in the target bucket
        // The bucket index determines which bit differs from our ID
        let byte_idx = bucket_idx / 8;
        let bit_idx = bucket_idx % 8;

        if byte_idx < 20 {
            // Flip the bit at the bucket position
            id[byte_idx] ^= 1 << (7 - bit_idx);

            // Randomize lower bits
            for byte in id.iter_mut().skip(byte_idx + 1) {
                *byte = rng.r#gen();
            }
        }

        id
    }

    fn bucket_index_for(&self, id: &[u8; 20]) -> usize {
        for i in (0..20).rev() {
            if id[i] != self.self_id[i] {
                return i * 8 + (7 - (id[i] ^ self.self_id[i]).leading_zeros() as usize);
            }
        }
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_routing_table_creation() {
        let table = RoutingTable::new([0u8; 20]);
        assert_eq!(table.total_node_count(), 0);
        assert!(table.get_bucket(0).is_some());
        assert!(table.get_bucket(159).is_some());
        assert!(table.get_bucket(160).is_none());
    }

    #[test]
    fn test_insert_and_find() {
        let mut table = RoutingTable::new([0x80u8; 20]);
        let node = DhtNode::new([0xFFu8; 20], "127.0.0.1:6881".parse().unwrap());
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
        table.insert(DhtNode::new(id, "127.0.0.1:6881".parse().unwrap()));
        assert!(table.remove(&id));
        assert_eq!(table.total_node_count(), 0);
    }

    #[test]
    fn test_eviction() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 0..10u8 {
            let mut node = DhtNode::new([i; 20], "127.0.0.1:6881".parse().unwrap());
            for _ in 0..3 {
                node.record_failure();
            }
            table.insert(node);
        }
        assert!(table.evict_bad_nodes() > 0);
    }

    #[test]
    fn test_mark_good() {
        let mut table = RoutingTable::new([0u8; 20]);
        let id = [1u8; 20];
        let mut node = DhtNode::new(id, "127.0.0.1:6881".parse().unwrap());
        node.record_failure();
        node.record_failure();
        table.insert(node);

        // Mark as good should reset failure count
        assert!(table.mark_good(&id));
        let closest = table.find_closest(&id, 1);
        assert_eq!(closest.len(), 1);
        assert!(closest[0].is_good());
    }

    #[test]
    fn test_mark_bad() {
        let mut table = RoutingTable::new([0u8; 20]);
        let id = [2u8; 20];
        table.insert(DhtNode::new(id, "127.0.0.1:6881".parse().unwrap()));

        // Mark as bad multiple times
        assert!(table.mark_bad(&id));
        assert!(table.mark_bad(&id));
        assert!(table.mark_bad(&id));

        let closest = table.find_closest(&id, 1);
        assert_eq!(closest.len(), 1);
        assert!(closest[0].is_bad());
    }

    #[test]
    fn test_mark_questionable() {
        let mut table = RoutingTable::new([0u8; 20]);
        let id = [3u8; 20];
        table.insert(DhtNode::new(id, "127.0.0.1:6881".parse().unwrap()));

        // Mark as questionable (just checks existence in current implementation)
        assert!(table.mark_questionable(&id));
    }

    #[test]
    fn test_get_random_node() {
        let mut table = RoutingTable::new([0u8; 20]);
        for i in 0..5u8 {
            table.insert(DhtNode::new([i; 20], "127.0.0.1:6881".parse().unwrap()));
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
    fn test_questionable_and_bad_counts() {
        let mut table = RoutingTable::new([0u8; 20]);

        // Add a good node
        table.insert(DhtNode::new([1u8; 20], "127.0.0.1:6881".parse().unwrap()));

        // Add a bad node
        let mut bad_node = DhtNode::new([2u8; 20], "127.0.0.1:6882".parse().unwrap());
        for _ in 0..3 {
            bad_node.record_failure();
        }
        table.insert(bad_node);

        assert_eq!(table.bad_node_count(), 1);
        assert!(table.good_node_count() >= 1);
    }

    #[test]
    fn test_refresh_buckets() {
        let table = RoutingTable::new([0u8; 20]);
        let targets = table.refresh_buckets();
        // Initially all buckets need refresh, but they're empty
        // So we should get targets for buckets needing refresh
        assert!(targets.len() <= 160);
    }

    #[test]
    fn test_get_questionable_nodes() {
        let mut table = RoutingTable::new([0u8; 20]);

        // Add a node (it will be good initially)
        table.insert(DhtNode::new([1u8; 20], "127.0.0.1:6881".parse().unwrap()));

        // Get questionable nodes (should be empty since node is fresh)
        let questionable = table.get_questionable_nodes();
        assert!(questionable.is_empty());
    }

    #[test]
    fn test_fill_routing_table() {
        let table = RoutingTable::new([0u8; 20]);
        let targets = table.fill_routing_table();
        // All buckets are empty, so we should get targets to fill them
        assert!(!targets.is_empty());
        assert!(targets.len() <= 160);
    }

    #[test]
    fn test_generate_random_id_in_bucket() {
        let table = RoutingTable::new([0u8; 20]);

        // Generate IDs for different buckets
        for bucket_idx in [0, 50, 100, 159] {
            let id = table.generate_random_id_in_bucket(bucket_idx);
            // Verify the ID is different from our own ID
            assert_ne!(id, table.self_id);
        }
    }
}
