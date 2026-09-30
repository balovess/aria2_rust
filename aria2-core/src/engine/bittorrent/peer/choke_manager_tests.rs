//! Tests for the seeder-state choke policy used by the active seeding swarm.

#[cfg(test)]
pub(crate) mod tests {
    use std::time::{Duration, Instant};

    use crate::engine::bittorrent::peer::choke_manager::BtSeederStateChoke;
    use crate::engine::bittorrent::peer::stats::PeerStats;

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
    fn test_seeder_ranks_by_recent_upload_rate_after_old_burst_expires() {
        let now = Instant::now();
        let mut peers = [
            {
                let mut peer = make_peer();
                peer.peer_interested = true;
                peer.last_unchoke_at = now - Duration::from_secs(60);
                peer.upload_speed = 100_000.0;
                peer.record_upload_rate_at(100_000, now - Duration::from_secs(11));
                peer
            },
            {
                let mut peer = make_peer();
                peer.peer_interested = true;
                peer.last_unchoke_at = now - Duration::from_secs(60);
                peer.upload_speed = 100.0;
                peer.record_upload_rate_at(100, now - Duration::from_secs(1));
                peer
            },
        ];

        assert!(
            peers[0].upload_speed > peers[1].upload_speed,
            "the expired historical EMA is intentionally larger"
        );
        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtSeederStateChoke::with_slots(1);
        choke.set_round(2);
        choke.execute_choke_at(&mut refs[..], now);

        assert!(!peers[1].am_choking, "recently uploaded peer ranks first");
        assert!(
            peers[0].am_choking,
            "expired upload burst must not win a slot"
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
}
