//! Tests for bt_choke_manager — seeder-state and leecher-state choking algorithms.

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, Instant};

    use crate::engine::bt_choke_manager::{BtLeecherStateChoke, BtSeederStateChoke};
    use crate::engine::peer_stats::PeerStats;

    fn make_peer() -> PeerStats {
        PeerStats::new([0u8; 20], "127.0.0.1:6881".parse().unwrap())
    }

    /// Convert a mutable slice of PeerStats into the `Vec<&mut PeerStats>`
    /// format required by `execute_choke`.
    fn to_choke_refs(peers: &mut [PeerStats]) -> Vec<&mut PeerStats> {
        peers.iter_mut().collect()
    }

    // -- Seeder-state tests --

    #[test]
    fn test_seeder_outstanding_upload_ranks_highest() {
        let mut peers = [
            {
                let mut p = make_peer();
                p.upload_speed = 50000.0;
                p.peer_interested = true;
                p
            },
            {
                let mut p = make_peer();
                p.upload_speed = 1000.0;
                p.peer_interested = true;
                p.outstanding_upload_count = 1;
                p
            },
        ];

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtSeederStateChoke::new();
        choke.execute_choke(&mut refs[..]);

        assert!(
            !peers[1].am_choking,
            "Peer with outstanding upload should be unchoked"
        );
    }

    #[test]
    fn test_seeder_recent_unchoking_beats_speed() {
        let now = Instant::now();
        let mut peers = [
            {
                let mut p = make_peer();
                p.upload_speed = 100000.0;
                p.peer_interested = true;
                p.last_unchoke_at = now - Duration::from_secs(60);
                p
            },
            {
                let mut p = make_peer();
                p.upload_speed = 500.0;
                p.peer_interested = true;
                p.last_unchoke_at = now - Duration::from_secs(5);
                p
            },
        ];

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtSeederStateChoke::with_slots(2);
        choke.execute_choke(&mut refs[..]);

        assert!(
            !peers[1].am_choking,
            "Recently unchoked peer should be unchoked"
        );
    }

    #[test]
    fn test_seeder_optimistic_unchoke_rounds_0_1() {
        let mut peers: Vec<PeerStats> = (0..6)
            .map(|_| {
                let mut p = make_peer();
                p.peer_interested = true;
                p
            })
            .collect();

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtSeederStateChoke::with_slots(3);
        choke.execute_choke(&mut refs[..]);

        let opt_count = peers.iter().filter(|p| p.opt_unchoking).count();
        assert!(
            opt_count <= 1,
            "At most one peer should be optimistically unchoked"
        );

        let unchoked_count = peers.iter().filter(|p| !p.am_choking).count();
        assert!(
            unchoked_count >= 3,
            "At least 3 peers should be unchoked (regular + optional optimistic)"
        );
    }

    #[test]
    fn test_seeder_round_cycle() {
        let mut choke = BtSeederStateChoke::new();
        assert_eq!(choke.round(), 0);

        let mut peers = {
            let mut p = make_peer();
            p.peer_interested = true;
            [p]
        };

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke(&mut refs[..]);
        assert_eq!(choke.round(), 1);

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke(&mut refs[..]);
        assert_eq!(choke.round(), 2);

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke(&mut refs[..]);
        assert_eq!(choke.round(), 0); // wraps back to 0
    }

    #[test]
    fn test_seeder_not_interested_peers_choked() {
        let mut peers = [
            {
                let mut p = make_peer();
                p.peer_interested = true;
                p
            },
            {
                let mut p = make_peer();
                p.peer_interested = false;
                p
            },
        ];

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtSeederStateChoke::with_slots(2);
        choke.execute_choke(&mut refs[..]);

        assert!(!peers[0].am_choking, "Interested peer should be unchoked");
        assert!(peers[1].am_choking, "Not-interested peer should be choked");
        assert!(
            !peers[1].opt_unchoking,
            "Not-interested peer should not be optimistically unchoked"
        );
    }

    // -- Leecher-state tests --

    #[test]
    fn test_leecher_regular_unchoker_preferred() {
        let now = Instant::now();
        let mut peers = [
            {
                let mut p = make_peer();
                p.download_speed = 100000.0;
                p.peer_interested = true;
                p.last_data_time = None;
                p
            },
            {
                let mut p = make_peer();
                p.download_speed = 500.0;
                p.peer_interested = true;
                p.last_data_time = Some(now - Duration::from_secs(5));
                p
            },
        ];

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtLeecherStateChoke::new();
        choke.set_round(1);
        choke.execute_choke(&mut refs[..]);

        assert!(
            !peers[1].am_choking,
            "Regular unchoker peer should be unchoked"
        );
    }

    #[test]
    fn test_leecher_snubbed_peers_excluded() {
        let mut peers = [
            {
                let mut p = make_peer();
                p.peer_interested = true;
                p.is_snubbed = true;
                p
            },
            {
                let mut p = make_peer();
                p.peer_interested = true;
                p.is_snubbed = false;
                p
            },
        ];

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtLeecherStateChoke::new();
        choke.set_round(1);
        choke.execute_choke(&mut refs[..]);

        assert!(
            peers[0].am_choking,
            "Snubbed peer should remain choked"
        );
        assert!(
            !peers[0].opt_unchoking,
            "Snubbed peer should not be optimistically unchoked"
        );
    }

    #[test]
    fn test_leecher_round_cycle() {
        let mut choke = BtLeecherStateChoke::new();
        assert_eq!(choke.round(), 0);

        let mut peers = {
            let mut p = make_peer();
            p.peer_interested = true;
            [p]
        };

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke(&mut refs[..]);
        assert_eq!(choke.round(), 1);

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke(&mut refs[..]);
        assert_eq!(choke.round(), 2);

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke(&mut refs[..]);
        assert_eq!(choke.round(), 0); // wraps back to 0
    }

    #[test]
    fn test_leecher_not_interested_peers_skipped() {
        let mut peers = [
            {
                let mut p = make_peer();
                p.peer_interested = false;
                p
            },
            {
                let mut p = make_peer();
                p.peer_interested = true;
                p
            },
        ];

        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtLeecherStateChoke::new();
        choke.set_round(1);
        choke.execute_choke(&mut refs[..]);

        assert!(
            peers[0].am_choking,
            "Not-interested peer should remain choked"
        );
    }
}
