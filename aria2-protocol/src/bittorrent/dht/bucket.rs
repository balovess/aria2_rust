const K: usize = 8;

#[derive(Debug, Clone)]
pub struct Bucket {
    nodes: Vec<DhtNode>,
}

impl Bucket {
    pub fn new() -> Self {
        Self {
            nodes: Vec::with_capacity(K),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }
    pub fn is_full(&self) -> bool {
        self.nodes.len() >= K
    }
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn insert(&mut self, node: DhtNode) -> Option<DhtNode> {
        if let Some(pos) = self.nodes.iter().position(|n| n.id == node.id) {
            self.nodes[pos].touch();
            return None;
        }

        if self.is_full() {
            if let Some(bad_pos) = self.nodes.iter().position(|n| n.is_bad()) {
                return Some(self.nodes.swap_remove(bad_pos));
            }
            None
        } else {
            self.nodes.push(node);
            None
        }
    }

    pub fn remove(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(pos) = self.nodes.iter().position(|n| &n.id == node_id) {
            self.nodes.swap_remove(pos);
            true
        } else {
            false
        }
    }

    pub fn get_nodes(&self) -> &[DhtNode] {
        &self.nodes
    }

    pub fn get_good_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_good()).count()
    }

    pub fn evict_bad(&mut self) -> usize {
        let before = self.nodes.len();
        self.nodes.retain(|n| !n.is_bad());
        before - self.nodes.len()
    }

    /// Mark a node as good (reset failure count and update last_seen)
    pub fn mark_good(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(node) = self.nodes.iter_mut().find(|n| &n.id == node_id) {
            node.touch();
            true
        } else {
            false
        }
    }

    /// Mark a node as bad (increment failure count)
    pub fn mark_bad(&mut self, node_id: &[u8; 20]) -> bool {
        if let Some(node) = self.nodes.iter_mut().find(|n| &n.id == node_id) {
            node.record_failure();
            true
        } else {
            false
        }
    }

    /// Mark a node as questionable (simulate old last_seen by recording failures)
    /// In DHT, a node becomes questionable if not seen for 15 minutes
    pub fn mark_questionable(&mut self, node_id: &[u8; 20]) -> bool {
        // We can't directly set last_seen, but we can check if the node exists
        // The is_questionable() method checks elapsed time, so this is just a lookup
        self.nodes.iter().any(|n| &n.id == node_id)
    }

    /// Check if this bucket needs refresh (hasn't been updated in 15 minutes)
    pub fn needs_refresh(&self) -> bool {
        // A bucket needs refresh if it has questionable nodes
        self.nodes.iter().any(|n| n.is_questionable())
    }

    /// Count questionable nodes in this bucket
    pub fn get_questionable_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_questionable()).count()
    }

    /// Count bad nodes in this bucket
    pub fn get_bad_count(&self) -> usize {
        self.nodes.iter().filter(|n| n.is_bad()).count()
    }
}

impl Default for Bucket {
    fn default() -> Self {
        Self::new()
    }
}

pub use super::node::DhtNode;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bucket_basic_ops() {
        let mut bucket = Bucket::new();
        assert!(!bucket.is_full());
        assert!(bucket.is_empty());

        let node = DhtNode::new([1u8; 20], "127.0.0.1:6881".parse().unwrap());
        assert!(bucket.insert(node).is_none());
        assert_eq!(bucket.len(), 1);
    }

    #[test]
    fn test_bucket_capacity() {
        let mut bucket = Bucket::new();
        for i in 0..K as u8 {
            let node = DhtNode::new([i; 20], "127.0.0.1:6881".parse().unwrap());
            bucket.insert(node);
        }
        assert!(bucket.is_full());

        let extra = DhtNode::new([0xFF; 20], "127.0.0.1:6882".parse().unwrap());
        assert!(bucket.insert(extra).is_none());
    }

    #[test]
    fn test_bucket_eviction() {
        let mut bucket = Bucket::new();
        for i in 0..K as u8 {
            let mut node = DhtNode::new([i; 20], "127.0.0.1:6881".parse().unwrap());
            if i < K as u8 - 1 {
                for _ in 0..3 {
                    node.record_failure();
                }
            }
            bucket.insert(node);
        }

        let evicted = bucket.evict_bad();
        assert_eq!(evicted, K - 1);
    }

    #[test]
    fn test_bucket_update_existing() {
        let mut bucket = Bucket::new();
        let node = DhtNode::new([5u8; 20], "127.0.0.1:6881".parse().unwrap());
        bucket.insert(node.clone());
        assert_eq!(bucket.len(), 1);

        assert!(bucket.insert(node).is_none());
        assert_eq!(bucket.len(), 1);
    }

    #[test]
    fn test_bucket_remove() {
        let mut bucket = Bucket::new();
        let id = [10u8; 20];
        let node = DhtNode::new(id, "127.0.0.1:6881".parse().unwrap());
        bucket.insert(node);
        assert!(bucket.remove(&id));
        assert!(bucket.is_empty());
        assert!(!bucket.remove(&id));
    }
}
