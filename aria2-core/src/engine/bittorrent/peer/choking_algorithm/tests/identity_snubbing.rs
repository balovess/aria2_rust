use super::*;

#[test]
fn test_snubbed_peers_get_lowered_score() {
    // Create two identical peers except one is snubbed
    let normal_peer = create_test_peer(50000.0, 500.0, true, true);
    let mut snubbed_peer = create_test_peer(50000.0, 500.0, true, true);
    snubbed_peer.is_snubbed = true;

    let normal_score = ChokingAlgorithm::calculate_peer_score(&normal_peer, false);
    let snubbed_score_stats = ChokingAlgorithm::calculate_peer_score(&snubbed_peer, false);
    let snubbed_score_explicit = ChokingAlgorithm::calculate_peer_score(&normal_peer, true);

    // Snubbed peer should have much lower score (penalty of -1000)
    assert!(snubbed_score_stats < normal_score);
    assert!(
        (normal_score - snubbed_score_stats) > 900.0,
        "Expected large score difference due to PeerStats snubbed penalty"
    );

    // Explicitly snubbed peer should also have much lower score
    assert!(snubbed_score_explicit < normal_score);
    assert!(
        (normal_score - snubbed_score_explicit) > 900.0,
        "Expected large score difference due to explicit snubbed penalty"
    );
}

#[test]
fn test_check_snubbed_returns_timed_out_peers() {
    let config = ChokingConfig {
        snubbed_timeout_secs: 1, // Use 1 second for testing
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Create a peer that hasn't received data
    let _addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    let peer = PeerStats::new([0u8; 20], "127.0.0.1:6882".parse().unwrap());
    algo.add_peer(peer);

    // Wait for timeout (slightly longer than snubbed_timeout_secs)
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Check snubbed status
    let snubbed_indices = algo.check_snubbed_peers();

    // Should flag peer 0 as snubbed
    assert_eq!(snubbed_indices.len(), 1);
    assert_eq!(snubbed_indices[0], 0);
    assert!(algo.get_peer(0).unwrap().is_snubbed);
}

#[test]
fn test_on_data_received_resets_snubbed_status() {
    let config = ChokingConfig {
        snubbed_timeout_secs: 1,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    let peer = PeerStats::new([0u8; 20], "127.0.0.1:6883".parse().unwrap());
    algo.add_peer(peer);

    // Wait for timeout
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Mark as snubbed
    let snubbed = algo.check_snubbed_peers();
    assert_eq!(snubbed.len(), 1);
    assert!(algo.get_peer(0).unwrap().is_snubbed);

    // Now receive data
    algo.on_data_received(0, 1024);

    // Snubbed status should be reset
    assert!(!algo.get_peer(0).unwrap().is_snubbed);
}

#[test]
fn test_get_peer_accessors() {
    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    let peer = PeerStats::new([0u8; 20], addr);
    algo.add_peer(peer);

    // Test immutable access
    assert!(algo.get_peer(0).is_some());
    assert!(algo.get_peer(1).is_none());

    // Test mutable access
    {
        let p = algo.get_peer_mut(0).unwrap();
        p.download_speed = 9999.0;
    }

    assert!((algo.get_peer(0).unwrap().download_speed - 9999.0).abs() < f64::EPSILON);
}

#[test]
fn test_config_defaults() {
    let config = ChokingConfig::default();

    assert_eq!(config.max_upload_slots, 4);
    assert_eq!(config.optimistic_unchoke_interval_secs, 30);
    assert_eq!(config.snubbed_timeout_secs, 60);
    assert_eq!(config.choke_rotation_interval_secs, 10);
}

// ==================== G1: Snubbing Enhancement Tests ====================

#[test]
fn test_snub_detection_after_timeout() {
    // Test that peers are detected as snubbed after timeout
    let config = ChokingConfig {
        snubbed_timeout_secs: 1,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    let peer = PeerStats::new([0u8; 20], "127.0.0.1:6882".parse().unwrap());
    algo.add_peer(peer);

    // Wait for timeout
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Check snubbed status - should detect via PeerStats timeout
    let snubbed_indices = algo.check_snubbed_peers();
    assert_eq!(snubbed_indices.len(), 1);
    assert!(algo.get_peer(0).unwrap().is_snubbed);
}

#[test]
fn test_snubbed_peer_always_remains_choked() {
    // Test that explicitly snubbed peers always get choked
    let config = ChokingConfig {
        max_upload_slots: 2,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Add 3 peers - peer 0 is high speed but will be snubbed
    algo.add_peer(create_test_peer(100000.0, 1000.0, true, true)); // Peer 0: high speed
    algo.add_peer(create_test_peer(50000.0, 500.0, true, true)); // Peer 1: medium speed
    algo.add_peer(create_test_peer(30000.0, 300.0, true, true)); // Peer 2: low speed

    // Explicitly snub peer 0 (the highest speed one)
    algo.mark_peer_snubbed(0);
    assert_eq!(algo.snubbed_count(), 1);

    // Run choke rotation - snubbed peer should be choked despite high score
    let peer0_identity = PeerIdentity::from(&algo.peers()[0]);
    let actions = algo.rotate_choke_by_identity();

    // Find action for peer 0 - it should be Choked or NoChange(if already choked)
    let peer0_action = actions.iter().find(|action| {
        action.identity() == peer0_identity
            && matches!(
                action,
                IdentityChokeAction::NoChange(_) | IdentityChokeAction::Choke(_)
            )
    });
    assert!(
        peer0_action.is_some(),
        "Peer 0 should have an action in results"
    );
    // Peer 0 started as choked (am_choking=true), so with -1000 score it stays choked
    match peer0_action.unwrap() {
        IdentityChokeAction::Choke(_) | IdentityChokeAction::NoChange(_) => {}
        IdentityChokeAction::Unchoke(_) => panic!("Snubbed peer 0 should NEVER be unchoked"),
    }
}

#[test]
fn test_unsnub_on_data_received() {
    // Test that receiving data from a peer auto-unsnubs them
    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    let peer = PeerStats::new([0u8; 20], "127.0.0.1:6883".parse().unwrap());
    algo.add_peer(peer);

    // Explicitly snub peer 0
    algo.mark_peer_snubbed(0);
    assert_eq!(algo.snubbed_count(), 1);

    // Receive data from peer 0 - should auto-unsnub
    algo.on_data_received(0, 1024);
    assert_eq!(algo.snubbed_count(), 0);
}

#[test]
fn test_identity_api_survives_peer_reordering() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig::default());
    let first = create_test_peer(1000.0, 100.0, true, true);
    let second = create_test_peer(2000.0, 200.0, true, true);
    let first_identity = PeerIdentity::from(&first);
    let second_identity = PeerIdentity::from(&second);
    algo.add_peer(first);
    algo.add_peer(second);

    algo.mark_peer_snubbed(0);
    algo.peers.swap(0, 1);
    assert!(algo.snubbed_peers.contains(&first_identity));
    algo.on_data_received_by_identity(first_identity, 4096);
    assert_eq!(algo.snubbed_count(), 0);

    algo.remove_peers_by_identity(&[second_identity]);
    assert_eq!(algo.peers().len(), 1);
    assert_eq!(PeerIdentity::from(&algo.peers()[0]), first_identity);
}

#[test]
fn test_identity_actions_match_registered_peers() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 2,
        ..Default::default()
    });
    let mut first = create_test_peer(3000.0, 0.0, true, true);
    first.last_data_time = Some(Instant::now());
    let mut second = create_test_peer(1000.0, 0.0, true, true);
    second.last_data_time = Some(Instant::now());
    let first_identity = PeerIdentity::from(&first);
    let second_identity = PeerIdentity::from(&second);
    algo.add_peer(first);
    algo.add_peer(second);

    let actions = algo.rotate_choke_by_identity();
    assert!(actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == first_identity)
    }));
    assert!(
        actions
            .iter()
            .any(|action| action.identity() == second_identity)
    );
}

#[test]
fn test_opt_unchoking_rotation_changes_peer() {
    // Test that optimistic unchoke rotates among eligible peers
    let config = ChokingConfig {
        optimistic_unchoke_interval_secs: 0, // Allow immediate re-selection
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Add 3 eligible peers (all choked + interested)
    let peers = [
        create_test_peer(1000.0, 100.0, true, true),
        create_test_peer(2000.0, 200.0, true, true),
        create_test_peer(3000.0, 300.0, true, true),
    ];
    let identities: Vec<_> = peers.iter().map(PeerIdentity::from).collect();
    for peer in peers {
        algo.add_peer(peer);
    }

    // First optimistic unchoke
    let first = algo.optimistically_unchoke_by_identity();
    assert!(first.is_some());
    let first_identity = first.unwrap();

    // Second optimistic unchoke - should pick a DIFFERENT peer (round-robin)
    // Reset the last_optimistic_unchoke time so they're eligible again
    for i in 0..3 {
        if let Some(p) = algo.get_peer_mut(i) {
            p.last_optimistic_unchoke_at =
                std::time::Instant::now() - std::time::Duration::from_secs(1);
        }
    }

    let second = algo.optimistically_unchoke_by_identity();
    assert!(second.is_some());
    let second_identity = second.unwrap();

    // With round-robin, second should differ from first (unless only 1 candidate)
    // Since all 3 are eligible and we use rotation, we expect different peer
    assert_ne!(
        first_identity, second_identity,
        "Optimistic unchoke should rotate to a different peer"
    );
    assert!(identities.contains(&first_identity));
    assert!(identities.contains(&second_identity));
}

#[test]
fn test_sync_live_peer_preserves_choking_rotation_state() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig::default());
    let mut tracked = create_test_peer(1000.0, 100.0, false, true);
    let identity = PeerIdentity::from(&tracked);
    let now = std::time::Instant::now();
    tracked.last_unchoke_at = now - std::time::Duration::from_secs(20);
    tracked.last_optimistic_unchoke_at = now - std::time::Duration::from_secs(25);
    tracked.opt_unchoking = true;
    tracked.is_snubbed = true;
    tracked.snub_count = 3;
    algo.add_peer(tracked);

    let mut live = algo.peers()[0].clone();
    assert_eq!(PeerIdentity::from(&live), identity);
    live.download_speed = 500.0;
    live.upload_speed = 50.0;
    live.downloaded_bytes = 2048;
    live.last_data_time = Some(now);
    live.last_unchoke_at = now;
    live.last_optimistic_unchoke_at = now;
    live.opt_unchoking = false;
    live.is_snubbed = false;
    live.snub_count = 0;
    algo.sync_peers_by_identity([&live]);

    let synced = algo.peers().first().unwrap();
    assert_eq!(synced.downloaded_bytes, 2048);
    assert_eq!(synced.last_data_time, Some(now));
    assert_eq!(
        synced.last_unchoke_at,
        now - std::time::Duration::from_secs(20)
    );
    assert_eq!(
        synced.last_optimistic_unchoke_at,
        now - std::time::Duration::from_secs(25)
    );
    assert!(synced.opt_unchoking);
    assert!(synced.is_snubbed);
    assert_eq!(synced.snub_count, 3);
}

#[test]
fn test_opt_unchoking_excludes_snubbed_peers() {
    // Test that snubbed peers are excluded from optimistic unchoke candidates
    let config = ChokingConfig {
        optimistic_unchoke_interval_secs: 0,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Peer 0: eligible but will be snubbed
    algo.add_peer(create_test_peer(5000.0, 500.0, true, true));
    // Peer 1: eligible and NOT snubbed
    let available = create_test_peer(3000.0, 300.0, true, true);
    let available_identity = PeerIdentity::from(&available);
    algo.add_peer(available);

    // Snub peer 0
    algo.mark_peer_snubbed(0);

    // Optimistic unchoke should ONLY select peer 1
    let result = algo.optimistically_unchoke_by_identity();
    assert!(result.is_some());
    assert_eq!(result, Some(available_identity));
}

#[test]
fn test_mark_snubbed_idempotent() {
    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    algo.add_peer(create_test_peer(100.0, 10.0, true, true));

    // Marking same peer twice should not increase count
    algo.mark_peer_snubbed(0);
    assert_eq!(algo.snubbed_count(), 1);
    algo.mark_peer_snubbed(0); // Duplicate
    assert_eq!(
        algo.snubbed_count(),
        1,
        "Duplicate mark should not increase count"
    );
}

#[test]
fn test_unsnub_non_snubbed_peer_returns_false() {
    let config = ChokingConfig::default();
    let mut algo = ChokingAlgorithm::new(config);

    algo.add_peer(create_test_peer(100.0, 10.0, true, true));

    // Unsnubbing a peer that was never snubbed returns false
    let result = algo.unsnub_peer(0);
    assert!(!result, "Unsnubbing non-snubbed peer should return false");
}
