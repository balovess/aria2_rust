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
        if bucket_idx >= BUCKET_COUNT { return; }

        match self.buckets[bucket_idx].insert(node) {
            Some(evicted) => {
                tracing::debug!("DHT节点被替换: {}", evicted.id_hex());
            }
            None => {}
        }
    }

    pub fn remove(&mut self, node_id: &[u8; 20]) -> bool {
        let idx = self.bucket_index_for(node_id);
        if idx >= BUCKET_COUNT { return false; }
        self.buckets[idx].remove(node_id)
    }

    pub fn find_closest(&self, target: &[u8; 20], count: usize) -> Vec<&DhtNode> {
        let mut all_nodes: Vec<(usize, &DhtNode)> = self.buckets.iter()
            .enumerate()
            .flat_map(|(i, b)| b.get_nodes().iter().map(move |n| (i, n)))
            .collect();

        all_nodes.sort_by_key(|(_, n)| n.distance_to(target));

        all_nodes.into_iter()
            .take(count)
            .map(|(_, n)| n)
            .collect()
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
            for _ in 0..3 { node.record_failure(); }
            table.insert(node);
        }
        assert!(table.evict_bad_nodes() > 0);
    }
}
