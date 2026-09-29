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

    pub(crate) fn available_slots(&self, active: usize) -> usize {
        if self.max_peers == 0 {
            self.batch_size
        } else {
            self.max_peers.saturating_sub(active).min(self.batch_size)
        }
    }

    pub(crate) fn max_concurrent_dials(&self) -> usize {
        self.batch_size
    }

    pub(crate) fn select_candidates(
        &self,
        candidates: &[PeerAddr],
        active: &HashSet<(String, u16)>,
        mut is_temporarily_rejected: impl FnMut(&str) -> bool,
    ) -> Vec<PeerAddr> {
        if self.available_slots(active.len()) == 0 {
            return Vec::new();
        }
        let mut seen = HashSet::new();
        candidates
            .iter()
            .filter(|peer| {
                let key = (peer.ip.clone(), peer.port);
                !active.contains(&key) && !is_temporarily_rejected(&peer.ip) && seen.insert(key)
            })
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
    fn retains_unique_candidates_beyond_the_dial_concurrency_limit() {
        let coordinator = BtPeerCoordinator::new(2, 3);
        let active = HashSet::from([("127.0.0.1".to_string(), 1)]);
        let candidates = vec![
            peer("127.0.0.1", 1),
            peer("127.0.0.1", 2),
            peer("127.0.0.1", 2),
            peer("127.0.0.1", 3),
            peer("127.0.0.1", 4),
            peer("127.0.0.1", 5),
        ];
        let selected = coordinator.select_candidates(&candidates, &active, |_| false);
        assert_eq!(selected.len(), 4);
        assert_eq!(selected[0].port, 2);
        assert_eq!(selected[1].port, 3);
        assert_eq!(selected[2].port, 4);
        assert_eq!(selected[3].port, 5);
        assert_eq!(coordinator.max_concurrent_dials(), 3);
    }

    #[test]
    fn selects_no_candidates_when_peer_limit_is_full() {
        let coordinator = BtPeerCoordinator::new(2, 3);
        let active = HashSet::from([("127.0.0.1".to_string(), 1), ("127.0.0.1".to_string(), 2)]);
        let candidates = vec![peer("127.0.0.1", 3)];

        assert!(
            coordinator
                .select_candidates(&candidates, &active, |_| false)
                .is_empty()
        );
    }

    #[test]
    fn rejected_candidates_do_not_consume_the_dial_window() {
        let coordinator = BtPeerCoordinator::new(2, 2);
        let active = HashSet::from([("127.0.0.1".to_string(), 1)]);
        let candidates = vec![
            peer("10.0.0.1", 2),
            peer("10.0.0.2", 3),
            peer("10.0.0.3", 4),
        ];

        let selected = coordinator.select_candidates(&candidates, &active, |ip| ip == "10.0.0.1");

        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].port, 3);
        assert_eq!(selected[1].port, 4);
    }
}
