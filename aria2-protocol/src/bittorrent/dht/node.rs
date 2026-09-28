use std::time::Instant;

#[derive(Debug, Clone)]
pub struct DhtNode {
    pub(crate) id: [u8; 20],
    pub(crate) addr: std::net::SocketAddr,
    pub(crate) last_seen: Instant,
    pub(crate) failed_count: u8,
    pub(crate) verified: bool,
}

impl DhtNode {
    pub fn new(id: [u8; 20], addr: std::net::SocketAddr) -> Self {
        Self {
            id,
            addr,
            last_seen: Instant::now(),
            failed_count: 0,
            verified: true,
        }
    }

    /// Create a discovered node that has not yet answered a valid query.
    pub(crate) fn unverified(id: [u8; 20], addr: std::net::SocketAddr) -> Self {
        Self {
            id,
            addr,
            last_seen: Instant::now(),
            failed_count: 0,
            verified: false,
        }
    }

    pub fn id(&self) -> &[u8; 20] {
        &self.id
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn last_seen(&self) -> Instant {
        self.last_seen
    }

    pub fn failed_count(&self) -> u8 {
        self.failed_count
    }

    pub fn is_good(&self) -> bool {
        self.verified && self.failed_count < 3 && self.last_seen.elapsed().as_secs() < 900
    }

    pub fn is_questionable(&self) -> bool {
        !self.is_bad() && (!self.verified || self.last_seen.elapsed().as_secs() >= 900)
    }

    pub fn is_bad(&self) -> bool {
        self.failed_count >= 3
    }

    pub fn touch(&mut self) {
        self.last_seen = Instant::now();
        self.failed_count = 0;
        self.verified = true;
    }

    pub fn record_failure(&mut self) {
        self.failed_count = self.failed_count.saturating_add(1);
    }

    pub fn distance_to(&self, target: &[u8; 20]) -> usize {
        let mut distance = 0usize;
        for (i, &t) in target.iter().enumerate().take(20) {
            let xor = self.id[i] ^ t;
            if xor != 0 {
                distance += (19 - i) * 8 + (7 - xor.leading_zeros()) as usize;
            }
        }
        distance
    }

    pub fn id_hex(&self) -> String {
        self.id.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

impl PartialEq for DhtNode {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for DhtNode {}

impl std::hash::Hash for DhtNode {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_creation() {
        let id = [1u8; 20];
        let addr = "127.0.0.1:6881".parse().unwrap();
        let node = DhtNode::new(id, addr);
        assert!(node.is_good());
        assert!(!node.is_bad());
        assert_eq!(node.id_hex().len(), 40);
    }

    #[test]
    fn discovered_node_is_questionable_until_a_valid_response() {
        let mut node = DhtNode::unverified([5u8; 20], "127.0.0.1:6881".parse().unwrap());

        assert!(!node.is_good());
        assert!(node.is_questionable());

        node.touch();
        assert!(node.is_good());
        assert!(!node.is_questionable());
    }

    #[test]
    fn test_node_failures() {
        let mut node = DhtNode::new([2u8; 20], "0.0.0.0:0".parse().unwrap());
        for _ in 0..3 {
            node.record_failure();
        }
        assert!(node.is_bad());
        assert!(!node.is_questionable());

        let mut good_node = DhtNode::new([3u8; 20], "0.0.0.0:0".parse().unwrap());
        for _ in 0..3 {
            good_node.record_failure();
        }
        good_node.touch();
        assert!(good_node.is_good());
    }

    #[test]
    fn repeated_failures_saturate_without_reviving_a_bad_node() {
        let mut node = DhtNode::new([4u8; 20], "0.0.0.0:0".parse().unwrap());

        for _ in 0..=u8::MAX {
            node.record_failure();
        }

        assert_eq!(node.failed_count(), u8::MAX);
        assert!(node.is_bad());
        assert!(!node.is_good());
    }

    #[test]
    fn test_distance_calculation() {
        let a = DhtNode::new([0xFFu8; 20], "0.0.0.0:0".parse().unwrap());
        let b: [u8; 20] = [0x00; 20];
        assert!(a.distance_to(&b) > 0);

        let same_id: [u8; 20] = [0xFF; 20];
        assert_eq!(a.distance_to(&same_id), 0);
    }
}
