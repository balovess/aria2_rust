use super::*;

#[test]
fn test_new_algorithm_empty() {
    let config = ChokingConfig::default();
    let algo = ChokingAlgorithm::new(config);

    assert!(algo.is_empty());
    assert_eq!(algo.len(), 0);
}

#[test]
fn choke_rotation_deadline_is_reset_by_each_decision() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig {
        choke_rotation_interval_secs: 10,
        ..ChokingConfig::default()
    });
    let due_at = algo.next_choke_rotation_deadline().unwrap();
    assert!(algo.choke_rotation_due(due_at));

    algo.rotate_choke_by_identity();
    assert!(!algo.choke_rotation_due(Instant::now()));

    algo.config.choke_rotation_interval_secs = 0;
    assert_eq!(algo.next_choke_rotation_deadline(), None);
}

#[test]
fn test_remove_peers_remaps_snubbed_and_optimistic_indices() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig::default());
    for port in 1..=3 {
        algo.add_peer(PeerStats::new(
            [port as u8; 20],
            format!("127.0.0.1:{port}").parse().unwrap(),
        ));
    }
    algo.mark_peer_snubbed(1);
    algo.current_optimistic_peer = Some(PeerIdentity {
        peer_id: [2u8; 20],
        addr: "127.0.0.1:2".parse().unwrap(),
    });

    algo.remove_peers(&[1]);

    assert_eq!(algo.len(), 2);
    assert_eq!(algo.snubbed_count(), 0);
    assert_eq!(algo.current_optimistic_peer, None);
}

#[test]
fn test_add_remove_peers() {
    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    // Add peers
    assert_eq!(algo.len(), 0);
    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    algo.add_peer(PeerStats::new([0u8; 20], addr));
    assert_eq!(algo.len(), 1);
    algo.add_peer(PeerStats::new([0u8; 20], addr));
    assert_eq!(algo.len(), 2);
    algo.add_peer(PeerStats::new([0u8; 20], addr));
    assert_eq!(algo.len(), 3);

    // Remove middle peer
    algo.remove_peers(&[1]);
    assert_eq!(algo.len(), 2);

    // Remove first peer
    algo.remove_peers(&[0]);
    assert_eq!(algo.len(), 1);

    // Remove last peer
    algo.remove_peers(&[0]);
    assert!(algo.is_empty());
}

#[test]
fn test_rotate_choke_selects_top_k() {
    let config = ChokingConfig {
        max_upload_slots: 4,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Add 6 peers with different speeds (all start choked)
    // Peer 0: highest download speed
    algo.add_peer(create_test_peer(100000.0, 1000.0, true, true));
    // Peer 1: medium-high
    algo.add_peer(create_test_peer(80000.0, 800.0, true, true));
    // Peer 2: medium
    algo.add_peer(create_test_peer(60000.0, 600.0, true, true));
    // Peer 3: medium-low
    algo.add_peer(create_test_peer(40000.0, 400.0, true, true));
    // Peer 4: low
    algo.add_peer(create_test_peer(20000.0, 200.0, true, true));
    // Peer 5: very low
    algo.add_peer(create_test_peer(10000.0, 100.0, true, true));

    let actions = algo.rotate_choke_by_identity();

    // Count unchoke actions
    let unchoke_count = actions
        .iter()
        .filter(|a| matches!(a, IdentityChokeAction::Unchoke(_)))
        .count();

    // Should have exactly 3 unchoke actions (top 3 by score)
    assert_eq!(unchoke_count, 3);

    // Verify all actions are accounted for
    assert_eq!(actions.len(), 6); // One action per peer
}

#[test]
fn test_regular_unchoke_requires_interest_and_reserves_optimistic_slot() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 2,
        optimistic_unchoke_interval_secs: 0,
        ..Default::default()
    });
    let mut interested = create_test_peer(1000.0, 0.0, true, true);
    let interested_identity = PeerIdentity::from(&interested);
    interested.last_data_time = Some(Instant::now());
    let mut optimistic_candidate = create_test_peer(500.0, 0.0, true, true);
    let optimistic_identity = PeerIdentity::from(&optimistic_candidate);
    optimistic_candidate.last_data_time = Some(Instant::now());
    let uninterested = create_test_peer(100_000.0, 0.0, true, false);
    let uninterested_identity = PeerIdentity::from(&uninterested);
    algo.add_peer(interested);
    algo.add_peer(optimistic_candidate);
    algo.add_peer(uninterested);

    let actions = algo.rotate_choke_by_identity();
    assert!(!actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == uninterested_identity)
    }));
    assert!(matches!(
        algo.optimistically_unchoke_by_identity(),
        Some(identity) if identity == interested_identity || identity == optimistic_identity
    ));
}

#[test]
fn test_first_optimistic_unchoke_does_not_wait_for_rotation_interval() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig::default());
    let peer = create_test_peer(1000.0, 0.0, true, true);
    let identity = PeerIdentity::from(&peer);
    algo.add_peer(peer);

    assert_eq!(algo.optimistically_unchoke_by_identity(), Some(identity));
}
