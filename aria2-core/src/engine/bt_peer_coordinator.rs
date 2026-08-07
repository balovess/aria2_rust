use aria2_protocol::bittorrent::peer::connection::PeerAddr;
use std::collections::HashSet;

/// Session-local connection coordinator for BitTorrent peer replenishment.
///
/// This owns the policy part of the C++ `ActivePeerConnectionCommand`:
/// connection admission is derived from the live count, the configured peer
/// limit, and the set of already active/candidate endpoints. Socket I/O and
/// peer lifecycle ownership remain with the download command.
#[derive(Debug)]
pub(crate) struct BtPeerCoordinator {
    max_peers: usize,
    batch_size: usize,
}

impl BtPeerCoordinator {
    pub(crate) fn new(max_peers: usize, batch_size: usize) -> Self {
        Self {
            max_peers,
            batch_size: batch_size.max(1),
        }
    }

    pub(crate) fn set_max_peers(&mut self, max_peers: usize) {
        self.max_peers = max_peers;
    }

    pub(crate) fn should_replenish(&self, active: usize) -> bool {
        self.max_peers == 0 || active < self.minimum_peers()
    }

    pub(crate) fn available_slots(&self, active: usize) -> usize {
        if self.max_peers == 0 {
            self.batch_size
        } else {
            self.max_peers.saturating_sub(active).min(self.batch_size)
        }
    }

    pub(crate) fn minimum_peers(&self) -> usize {
        if self.max_peers == 0 {
            0
        } else {
            (self.max_peers * 4 / 5).max(1)
        }
    }

    pub(crate) fn select_candidates(
        &self,
        candidates: &[PeerAddr],
        active: &HashSet<(String, u16)>,
    ) -> Vec<PeerAddr> {
        let limit = self.available_slots(active.len());
        let mut seen = HashSet::new();
        candidates
            .iter()
            .filter(|peer| {
                let key = (peer.ip.clone(), peer.port);
                !active.contains(&key) && seen.insert(key)
            })
            .take(limit)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(ip: &str, port: u16) -> PeerAddr {
        PeerAddr::new(ip, port)
    }

    #[test]
    fn computes_original_minimum_peer_threshold() {
        let coordinator = BtPeerCoordinator::new(10, 10);
        assert_eq!(coordinator.minimum_peers(), 8);
        assert!(coordinator.should_replenish(7));
        assert!(!coordinator.should_replenish(8));
    }

    #[test]
    fn selects_unique_candidates_with_remaining_slots() {
        let coordinator = BtPeerCoordinator::new(2, 10);
        let active = HashSet::from([("127.0.0.1".to_string(), 1)]);
        let candidates = vec![
            peer("127.0.0.1", 1),
            peer("127.0.0.1", 2),
            peer("127.0.0.1", 2),
        ];
        let selected = coordinator.select_candidates(&candidates, &active);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].port, 2);
    }
}
