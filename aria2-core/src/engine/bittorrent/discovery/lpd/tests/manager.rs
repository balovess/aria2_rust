use super::*;

// Test: LpdManager Operations
// =========================================================================

#[tokio::test]
async fn test_lpd_manager_register_unregister() {
    let manager = LpdManager::default();

    // Register a torrent
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .unwrap();

    // Check active hashes
    let active = manager.active_hashes.read().await;
    assert!(
        active.contains(test_info_hash()),
        "Torrent should be registered"
    );
    drop(active);

    // Unregister
    manager.unregister_torrent(test_info_hash()).await;

    let active = manager.active_hashes.read().await;
    assert!(
        !active.contains(test_info_hash()),
        "Torrent should be unregistered"
    );
    drop(active);

    assert!(
        !manager.peers.read().await.contains_key(test_info_hash()),
        "Peer registry entry should be removed with the torrent"
    );
}

#[tokio::test]
async fn test_lpd_manager_get_peers_empty_initially() {
    let manager = LpdManager::default();

    let peers = manager.get_peers_for(test_info_hash()).await;
    assert!(peers.is_empty(), "No peers should exist initially");
}

#[tokio::test]
async fn test_lpd_manager_update_and_get_peers() {
    let manager = LpdManager::default();

    // Register first
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .unwrap();

    // Add some discovered peers
    let new_peers = vec![
        LpdPeer::new(
            test_info_hash(),
            6881,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        ),
        LpdPeer::new(
            test_info_hash(),
            6882,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
        ),
        LpdPeer::new(
            test_info_hash(),
            6883,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
        ),
    ];

    manager.update_peers(test_info_hash(), new_peers).await;

    let peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(peers.len(), 3, "Should have 3 stored peers");
}

#[tokio::test]
async fn test_lpd_manager_refreshes_known_peer_without_eviction() {
    let manager = LpdManager::default();
    let info_hash = test_info_hash();
    manager.register_torrent(info_hash, false).await.unwrap();

    let address = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 49));
    manager
        .update_peers(
            info_hash,
            (0..super::super::MAX_PEERS_PER_HASH)
                .map(|index| {
                    LpdPeer::new(
                        info_hash,
                        6881,
                        IpAddr::V4(Ipv4Addr::new(10, 0, 0, index as u8)),
                    )
                })
                .collect(),
        )
        .await;

    let previous_seen = manager
        .get_peers_for(info_hash)
        .await
        .into_iter()
        .find(|peer| peer.addr == address)
        .expect("target peer should be stored")
        .last_seen;
    manager
        .update_peers(info_hash, vec![LpdPeer::new(info_hash, 6999, address)])
        .await;

    let peers = manager.get_peers_for(info_hash).await;
    assert_eq!(peers.len(), super::super::MAX_PEERS_PER_HASH);
    let refreshed = peers
        .iter()
        .find(|peer| peer.addr == address)
        .expect("refreshed peer should remain present");
    assert_eq!(refreshed.port, 6999);
    assert!(refreshed.last_seen > previous_seen);
}

#[tokio::test]
async fn test_lpd_manager_multiple_torrents_independent() {
    let manager = LpdManager::default();

    manager
        .register_torrent(test_info_hash(), false)
        .await
        .unwrap();
    manager
        .register_torrent(test_info_hash_2(), false)
        .await
        .unwrap();

    // Add peers to first torrent
    manager
        .update_peers(
            test_info_hash(),
            vec![LpdPeer::new(
                test_info_hash(),
                6881,
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            )],
        )
        .await;

    // Add peers to second torrent
    manager
        .update_peers(
            test_info_hash_2(),
            vec![LpdPeer::new(
                test_info_hash_2(),
                6999,
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            )],
        )
        .await;

    let peers1 = manager.get_peers_for(test_info_hash()).await;
    let peers2 = manager.get_peers_for(test_info_hash_2()).await;

    assert_eq!(peers1.len(), 1, "First torrent should have 1 peer");
    assert_eq!(peers2.len(), 1, "Second torrent should have 1 peer");
    assert_ne!(
        peers1[0].info_hash, peers2[0].info_hash,
        "Peers should be for different torrents"
    );
}

#[tokio::test]
async fn test_lpd_manager_cleanup_expired_peers() {
    let manager = LpdManager::default();
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .unwrap();

    // Add a peer that's immediately "expired" (max_age = 0)
    let peer = LpdPeer::new(test_info_hash(), 6881, test_ip());
    manager.update_peers(test_info_hash(), vec![peer]).await;

    // Clean up with zero tolerance
    let removed = manager.cleanup_expired_peers(Duration::ZERO).await;
    assert!(removed > 0, "Should remove expired peers");

    let remaining = manager.get_peers_for(test_info_hash()).await;
    assert!(remaining.is_empty(), "Expired peers should be removed");
}

#[tokio::test]
async fn test_lpd_manager_is_available() {
    let manager = LpdManager::default();
    assert!(
        manager.is_available(),
        "Default manager should be available"
    );
}

#[tokio::test]
async fn test_lpd_registration_rejects_zero_tcp_port() {
    let manager = LpdManager::default();

    let error = manager
        .register_torrent_with_port(test_info_hash(), false, 0)
        .await
        .expect_err("LPD must not register a torrent without a BT TCP port");

    assert!(error.contains("non-zero"));
    assert!(
        !manager
            .active_hashes
            .read()
            .await
            .contains(test_info_hash())
    );
    assert_eq!(manager.announce_port_for(test_info_hash()).await, None);
}

#[tokio::test]
async fn test_lpd_manual_announce_rejects_zero_tcp_port() {
    let manager = LpdManager::default();

    let error = manager
        .announce_torrent(test_info_hash(), 0)
        .await
        .expect_err("LPD must not announce without a BitTorrent TCP port");

    assert!(error.contains("non-zero"));
}

#[tokio::test]
async fn test_lpd_registration_keeps_each_torrents_tcp_port() {
    let manager = LpdManager::default();

    manager
        .register_torrent_with_port(test_info_hash(), false, 41_001)
        .await
        .expect("first torrent should register");
    manager
        .register_torrent_with_port(test_info_hash_2(), false, 41_002)
        .await
        .expect("second torrent should register");

    assert_eq!(
        manager.announce_port_for(test_info_hash()).await,
        Some(41_001)
    );
    assert_eq!(
        manager.announce_port_for(test_info_hash_2()).await,
        Some(41_002)
    );
}

#[tokio::test]
async fn test_lpd_background_announce_has_one_process_task() {
    let manager = LpdManager::with_interval(60).expect("LPD manager should initialize");
    manager
        .register_torrent_with_port(test_info_hash(), false, 41_003)
        .await
        .expect("torrent should register");

    assert!(manager.start_background_announce(0).is_none());
    assert!(manager.is_background_announce_running());

    assert!(
        manager.start_background_announce(0).is_none(),
        "duplicate start must not create another process-level announce task"
    );
    assert!(manager.is_background_announce_running());

    manager.stop_background_announce();
    assert!(!manager.is_background_announce_running());
}

#[tokio::test]
async fn test_lpd_private_torrent_rejected() {
    // Per BEP 0027, private torrents must NOT be announced via LPD
    let manager = LpdManager::default();

    // Public torrent should register fine
    let result = manager.register_torrent(test_info_hash(), false).await;
    assert!(result.is_ok(), "Public torrent should register for LPD");

    // Private torrent should be rejected
    let private_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let result = manager.register_torrent(private_hash, true).await;
    assert!(
        result.is_err(),
        "Private torrent should be rejected from LPD"
    );
    assert!(
        result.unwrap_err().contains("BEP 0027"),
        "Error should reference BEP 0027"
    );

    // Verify the private hash was NOT added to active set
    let active = manager.active_hashes.read().await;
    assert!(
        !active.contains(private_hash),
        "Private hash should not be in active LPD set"
    );
}
