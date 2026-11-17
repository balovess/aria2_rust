use super::*;

#[test]
fn test_add_and_get_peer() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0x11u8; 20];
    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();

    storage.add_peer(info_hash, addr);

    let peers = storage.get_peers(&info_hash);
    assert_eq!(peers, vec![addr]);
}

#[test]
fn test_get_peers_empty() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0x22u8; 20];

    let peers = storage.get_peers(&info_hash);

    assert!(peers.is_empty(), "unknown info_hash should yield no peers");
}

#[test]
fn test_add_peer_updates_existing() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0x33u8; 20];
    let addr: SocketAddr = "127.0.0.1:9000".parse().unwrap();

    storage.add_peer(info_hash, addr);
    // Re-announcing the same peer must refresh, not duplicate.
    storage.add_peer(info_hash, addr);

    let peers = storage.get_peers(&info_hash);
    assert_eq!(
        peers.len(),
        1,
        "duplicate add should not create a new entry"
    );
    assert_eq!(peers, vec![addr]);
    assert_eq!(storage.total_peer_count(), 1);
}

#[test]
fn test_peer_expiry() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0xBBu8; 20];
    let addr: SocketAddr = "127.0.0.1:6000".parse().unwrap();
    // 31 minutes ago: past the 30-minute TTL.
    let expired_ts = Instant::now() - Duration::from_secs(60 * 31);

    storage.add_peer_with_timestamp(info_hash, addr, expired_ts);

    let peers = storage.get_peers(&info_hash);
    assert!(peers.is_empty(), "peer older than TTL must not be returned");
}

#[test]
fn info_hashes_excludes_expired_peer_entries() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0x42u8; 20];
    let addr: SocketAddr = "127.0.0.1:6881".parse().unwrap();
    storage.add_peer_with_timestamp(
        info_hash,
        addr,
        Instant::now() - PEER_TTL - Duration::from_secs(1),
    );

    assert!(storage.info_hashes().is_empty());
    assert!(storage.get_peers(&info_hash).is_empty());
}

#[test]
fn test_max_peers_limit() {
    let storage = DhtPeerStorage::with_config(DhtPeerStorageConfig {
        max_info_hashes: 3,
        max_peers_per_info_hash: 3,
    });
    let info_hash = [0xAAu8; 20];
    // First peer gets an explicitly older timestamp so it is the eviction
    // target regardless of clock resolution.
    let first_addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
    let old_ts = Instant::now() - Duration::from_secs(60);
    storage.add_peer_with_timestamp(info_hash, first_addr, old_ts);
    // Add enough peers to exceed the cap by one.
    for port in 5001u16..=5003 {
        let addr: SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
        storage.add_peer(info_hash, addr);
    }

    let peers = storage.get_peers(&info_hash);

    assert_eq!(
        peers.len(),
        3,
        "peer count must respect the configured per-hash limit"
    );
    assert!(
        !peers.contains(&first_addr),
        "oldest peer should have been evicted"
    );
}

#[test]
fn global_hash_limit_evicts_the_least_recently_updated_swarm() {
    let storage = DhtPeerStorage::with_config(DhtPeerStorageConfig {
        max_info_hashes: 3,
        max_peers_per_info_hash: 50,
    });
    let hash_for = |value: u64| {
        let mut hash = [0u8; 20];
        hash[..8].copy_from_slice(&value.to_be_bytes());
        hash
    };

    storage.add_peer_with_timestamp(
        hash_for(0),
        "127.0.0.1:9999"
            .parse()
            .expect("expired peer address should parse"),
        Instant::now() - PEER_TTL - Duration::from_secs(1),
    );
    storage.cleanup_expired();

    for value in 1..=3 {
        let addr: SocketAddr = format!("127.0.0.1:{}", 10_000 + value)
            .parse()
            .expect("fixture peer address should parse");
        storage.add_peer(hash_for(value), addr);
    }

    storage.add_peer(
        hash_for(1),
        "127.0.0.1:20000"
            .parse()
            .expect("refreshed peer address should parse"),
    );
    storage.add_peer(
        hash_for(4),
        "127.0.0.1:20001"
            .parse()
            .expect("new peer address should parse"),
    );

    let hashes = storage.info_hashes();
    assert_eq!(hashes.len(), 3);
    assert!(hashes.contains(&hash_for(1)), "refreshed swarm must remain");
    assert!(
        !hashes.contains(&hash_for(2)),
        "least recently updated swarm must be evicted"
    );
    assert!(hashes.contains(&hash_for(4)));
}

#[test]
fn updating_limits_immediately_trims_oldest_peers_and_swarms() {
    let storage = DhtPeerStorage::with_config(DhtPeerStorageConfig {
        max_info_hashes: 4,
        max_peers_per_info_hash: 3,
    });
    let hash_for = |value: u8| [value; 20];
    let old = Instant::now() - Duration::from_secs(10);
    for value in 1..=3 {
        storage.add_peer_with_timestamp(
            hash_for(value),
            format!("127.0.0.1:{}", 6_000 + u16::from(value))
                .parse()
                .unwrap(),
            old + Duration::from_secs(u64::from(value)),
        );
    }
    let hash = hash_for(3);
    for port in 6_010..=6_011 {
        storage.add_peer(hash, format!("127.0.0.1:{port}").parse().unwrap());
    }

    storage.set_config(DhtPeerStorageConfig {
        max_info_hashes: 2,
        max_peers_per_info_hash: 2,
    });

    let hashes = storage.info_hashes();
    assert_eq!(hashes.len(), 2);
    assert!(!hashes.contains(&hash_for(1)));
    assert!(hashes.contains(&hash_for(2)));
    assert_eq!(storage.get_peers(&hash).len(), 2);
    assert_eq!(storage.stats().max_info_hashes, 2);
}

#[test]
fn test_cleanup_expired() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0xCCu8; 20];
    let expired_addr: SocketAddr = "127.0.0.1:7000".parse().unwrap();
    let fresh_addr: SocketAddr = "127.0.0.1:7001".parse().unwrap();
    let expired_ts = Instant::now() - Duration::from_secs(60 * 31);

    storage.add_peer_with_timestamp(info_hash, expired_addr, expired_ts);
    storage.add_peer(info_hash, fresh_addr);

    assert_eq!(
        storage.total_peer_count(),
        2,
        "before cleanup both peers are stored"
    );

    storage.cleanup_expired();

    assert_eq!(
        storage.total_peer_count(),
        1,
        "after cleanup only the fresh peer remains"
    );
    let peers = storage.get_peers(&info_hash);
    assert_eq!(peers, vec![fresh_addr]);
}

#[test]
fn test_cleanup_removes_empty_info_hash() {
    let storage = DhtPeerStorage::new();
    let info_hash = [0xDDu8; 20];
    let addr: SocketAddr = "127.0.0.1:7100".parse().unwrap();
    let expired_ts = Instant::now() - Duration::from_secs(60 * 31);

    storage.add_peer_with_timestamp(info_hash, addr, expired_ts);
    assert_eq!(storage.total_peer_count(), 1);

    storage.cleanup_expired();

    assert_eq!(storage.total_peer_count(), 0);
    // The info_hash entry itself should be gone.
    assert!(storage.get_peers(&info_hash).is_empty());
}

#[test]
fn test_multiple_info_hashes() {
    let storage = DhtPeerStorage::new();
    let hash_a = [0x01u8; 20];
    let hash_b = [0x02u8; 20];
    let addr_a: SocketAddr = "127.0.0.1:8000".parse().unwrap();
    let addr_b: SocketAddr = "127.0.0.1:8001".parse().unwrap();

    storage.add_peer(hash_a, addr_a);
    storage.add_peer(hash_b, addr_b);

    let peers_a = storage.get_peers(&hash_a);
    let peers_b = storage.get_peers(&hash_b);

    assert_eq!(peers_a, vec![addr_a], "hash_a should only contain addr_a");
    assert_eq!(peers_b, vec![addr_b], "hash_b should only contain addr_b");
    assert_eq!(storage.total_peer_count(), 2);
}

#[test]
fn test_default_impl() {
    let storage = DhtPeerStorage::default();
    assert_eq!(storage.total_peer_count(), 0);
}
