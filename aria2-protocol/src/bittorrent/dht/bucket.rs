const K: usize = 8;
#[allow(dead_code)]
const BUCKET_COUNT: usize = 160;

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
