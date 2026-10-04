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
    fn zero_seeder_upload_slots_never_optimistically_unchokes() {
        let mut peer = make_peer();
        peer.peer_interested = true;
        let mut peers = [peer];
        let mut refs = to_choke_refs(&mut peers);
        let mut choke = BtSeederStateChoke::with_slots(0);

        choke.execute_choke(&mut refs[..]);

        assert!(peers[0].am_choking);
        assert!(!peers[0].opt_unchoking);
    }

    #[test]
    fn seeder_optimistic_slot_holds_until_rotation_deadline_then_advances() {
        let start = Instant::now();
        let mut peers = [make_peer(), make_peer()];
        for (index, peer) in peers.iter_mut().enumerate() {
            peer.peer_id[0] = index as u8 + 1;
            peer.addr.set_port(6881 + index as u16);
            peer.peer_interested = true;
        }
        let mut choke = BtSeederStateChoke::with_slots_and_optimistic_unchoke_interval(1, 30);

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke_at(&mut refs, start);
        let first = peers
            .iter()
            .position(|peer| peer.opt_unchoking)
            .expect("first optimistic slot");
        let other = 1 - first;

        peers[other].peer_interested = false;
        let sentinel = start - Duration::from_secs(5);
        peers[first].last_optimistic_unchoke_at = sentinel;
        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke_at(&mut refs, start + Duration::from_secs(10));

        assert!(peers[first].opt_unchoking);
        assert_eq!(
            peers[first].last_optimistic_unchoke_at, sentinel,
            "holding the current slot must not restart its rotation deadline"
        );

        peers[other].peer_interested = true;
        peers[other].upload_speed = 100_000.0;
        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke_at(&mut refs, start + Duration::from_secs(20));
        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke_at(&mut refs, start + Duration::from_secs(30));

        assert!(peers[other].opt_unchoking);
        assert!(!peers[first].opt_unchoking);
    }

    #[test]
    fn seeder_refills_optimistic_slot_when_incumbent_ranks_into_regular_slots() {
        let start = Instant::now();
        let mut peers = [make_peer(), make_peer(), make_peer(), make_peer()];
        for (index, peer) in peers.iter_mut().enumerate() {
            peer.peer_id[0] = index as u8 + 1;
            peer.addr.set_port(6881 + index as u16);
            peer.peer_interested = true;
            peer.last_unchoke_at = start - Duration::from_secs(60);
        }
        let mut choke = BtSeederStateChoke::with_slots_and_optimistic_unchoke_interval(3, 60);

        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke_at(&mut refs, start);
        let optimistic = peers
            .iter()
            .position(|peer| peer.opt_unchoking)
            .expect("initial optimistic slot");

        for elapsed in [10, 20] {
            let mut refs = to_choke_refs(&mut peers);
            choke.execute_choke_at(&mut refs, start + Duration::from_secs(elapsed));
        }
        let promotion_time = start + Duration::from_secs(30);
        peers[optimistic].record_upload_rate_at(4096, promotion_time - Duration::from_secs(1));
        let mut refs = to_choke_refs(&mut peers);
        choke.execute_choke_at(&mut refs, promotion_time);

        assert!(!peers[optimistic].am_choking);
        assert_eq!(
            peers.iter().filter(|peer| !peer.am_choking).count(),
            3,
            "promoting the optimistic peer must not leave an upload slot idle"
        );
        assert_eq!(peers.iter().filter(|peer| peer.opt_unchoking).count(), 1);
        assert!(!peers[optimistic].opt_unchoking);
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
