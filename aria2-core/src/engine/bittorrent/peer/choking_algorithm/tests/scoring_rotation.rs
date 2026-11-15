use super::*;

#[test]
fn test_recent_download_data_precedes_higher_speed_regular_candidate() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 2,
        ..Default::default()
    });
    let stale_fast = create_test_peer(100_000.0, 0.0, true, true);
    let stale_identity = PeerIdentity::from(&stale_fast);
    let mut recent_slow = create_test_peer(100.0, 0.0, true, true);
    let recent_identity = PeerIdentity::from(&recent_slow);
    recent_slow.last_data_time = Some(std::time::Instant::now());
    algo.add_peer(stale_fast);
    algo.add_peer(recent_slow);

    let actions = algo.rotate_choke_by_identity();
    assert!(actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == recent_identity)
    }));
    assert!(!actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == stale_identity)
    }));
}

#[test]
fn leecher_rotation_ranks_recent_download_rate_after_old_burst_expires() {
    let now = Instant::now();
    let mut algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 4,
        ..Default::default()
    });

    let mut expired_burst = create_test_peer(100_000.0, 0.0, true, true);
    let expired_identity = PeerIdentity::from(&expired_burst);
    expired_burst.last_data_time = Some(now - Duration::from_secs(11));
    expired_burst.record_download_rate_at(1_100_000, now - Duration::from_secs(11));

    assert!(expired_burst.download_speed > 300.0);
    algo.add_peer(expired_burst);

    let mut recent_identities = Vec::new();
    for speed in [300.0, 200.0, 100.0] {
        let mut peer = create_test_peer(speed, 0.0, true, true);
        peer.last_data_time = Some(now - Duration::from_secs(1));
        peer.record_download_rate_at(speed as u64, now - Duration::from_secs(1));
        recent_identities.push(PeerIdentity::from(&peer));
        algo.add_peer(peer);
    }

    let actions = algo.rotate_choke_by_identity();

    assert!(!actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == expired_identity)
    }));
    for identity in recent_identities {
        assert!(actions.iter().any(|action| {
            matches!(action, IdentityChokeAction::Unchoke(candidate) if *candidate == identity)
        }));
    }
}

#[test]
fn leecher_uninterested_stale_peer_consumes_regular_slot() {
    use rand::SeedableRng;

    let mut observed_uninterested_slot = false;
    for seed in 0..128 {
        let mut algo = ChokingAlgorithm::new(ChokingConfig {
            max_upload_slots: 3,
            ..Default::default()
        });
        let mut recent = create_test_peer(900.0, 0.0, true, true);
        recent.last_data_time = Some(Instant::now());
        let recent_identity = PeerIdentity::from(&recent);
        let stale_uninterested = create_test_peer(50.0, 0.0, true, false);
        algo.add_peer(recent);
        algo.add_peer(create_test_peer(100.0, 0.0, true, true));
        algo.add_peer(stale_uninterested);
        algo.add_peer(create_test_peer(10.0, 0.0, true, true));

        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let actions =
            super::super::selection::rotate_choke_by_identity_with_rng(&mut algo, &mut rng);
        let unchoked: std::collections::HashSet<_> = actions
            .iter()
            .filter_map(|action| match action {
                IdentityChokeAction::Unchoke(identity) => Some(identity),
                _ => None,
            })
            .collect();
        if unchoked.len() == 1 {
            assert!(unchoked.contains(&recent_identity));
            observed_uninterested_slot = true;
            break;
        }
    }
    assert!(
        observed_uninterested_slot,
        "an uninterested peer in the shuffled stale tail must consume a regular slot"
    );
}

#[test]
fn test_regular_ranking_uses_download_speed_not_upload_speed() {
    let mut algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 2,
        ..Default::default()
    });
    let mut faster_downloader = create_test_peer(20_000.0, 0.0, true, true);
    let faster_identity = PeerIdentity::from(&faster_downloader);
    faster_downloader.last_data_time = Some(std::time::Instant::now());
    let mut faster_uploader = create_test_peer(10_000.0, 1_000_000.0, true, true);
    let uploader_identity = PeerIdentity::from(&faster_uploader);
    faster_uploader.last_data_time = Some(std::time::Instant::now());
    algo.add_peer(faster_downloader);
    algo.add_peer(faster_uploader);

    let actions = algo.rotate_choke_by_identity();
    assert!(actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == faster_identity)
    }));
    assert!(!actions.iter().any(|action| {
        matches!(action, IdentityChokeAction::Unchoke(identity) if *identity == uploader_identity)
    }));
}

#[test]
fn test_rotate_choke_minimizes_changes() {
    let config = ChokingConfig {
        max_upload_slots: 3,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Add 4 peers
    for (speed, upload_speed, am_choking) in [
        (100000.0, 1000.0, false),
        (80000.0, 800.0, false),
        (60000.0, 600.0, true),
        (40000.0, 400.0, true),
    ] {
        let mut peer = create_test_peer(speed, upload_speed, am_choking, true);
        peer.last_data_time = Some(Instant::now());
        algo.add_peer(peer);
    }

    // First rotation: top 2 should stay unchoked (they're already there)
    let actions = algo.rotate_choke_by_identity();

    // Count NoChange actions for the already-unchoked peers
    let no_change_count = actions
        .iter()
        .filter(|a| matches!(a, IdentityChokeAction::NoChange(_)))
        .count();

    // At least the top 2 should have NoChange (they were already unchoked and remain so)
    assert!(
        no_change_count >= 2,
        "Expected at least 2 NoChange actions, got {}",
        no_change_count
    );

    // Second rotation without changes: should produce mostly NoChange
    let actions2 = algo.rotate_choke_by_identity();
    let no_change_count2 = actions2
        .iter()
        .filter(|a| matches!(a, IdentityChokeAction::NoChange(_)))
        .count();

    // All should be NoChange on second call (idempotent-safe)
    assert_eq!(no_change_count2, 4, "Expected all NoChange on second call");
}

#[test]
fn test_optimistically_unchoke_selects_choked_peer() {
    let config = ChokingConfig {
        optimistic_unchoke_interval_secs: 0, // Allow immediate optimistic unchoke for testing
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Add peers
    let first = create_test_peer(1000.0, 100.0, true, true);
    let first_identity = PeerIdentity::from(&first);
    algo.add_peer(first); // Choked + interested
    algo.add_peer(create_test_peer(2000.0, 200.0, false, true)); // Unchoked
    algo.add_peer(create_test_peer(3000.0, 300.0, true, false)); // Not interested

    let result = algo.optimistically_unchoke_by_identity();

    // Should select peer 0 (only one that meets criteria)
    assert!(
        result.is_some(),
        "Expected to select a peer for optimistic unchoke"
    );
    assert_eq!(result, Some(first_identity));
}

#[test]
fn test_optimistically_avoids_recent() {
    let config = ChokingConfig {
        optimistic_unchoke_interval_secs: 30,
        ..Default::default()
    };
    let mut algo = ChokingAlgorithm::new(config);

    // Add peer and mark it as recently optimistically unchoked
    let mut peer = create_test_peer(1000.0, 100.0, true, true);
    peer.record_optimistic_unchoke(); // Just marked, so < 30s ago
    algo.add_peer(peer);

    let result = algo.optimistically_unchoke_by_identity();

    // Should not select this peer (too recent)
    assert!(result.is_none());
}
