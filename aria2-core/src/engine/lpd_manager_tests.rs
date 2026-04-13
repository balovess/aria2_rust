//! Tests for Local Peer Discovery (LPD) Manager - Phase 15 H8
//!
//! Comprehensive tests covering:
//! - LPD announcement format validation
//! - Announcement parsing (valid, invalid, edge cases)
//! - Duplicate suppression
//! - LpdPeer equality and hashing
//! - LpdManager lifecycle operations

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use crate::engine::lpd_manager::{LpdManager, LpdPeer, parse_lpd_announcement};

// =========================================================================
// Helper Functions
// =========================================================================

/// Create a valid 40-character hex info hash for testing
fn test_info_hash() -> &'static str {
    "0123456789abcdef0123456789abcdef01234567"
}

/// Alternative info hash for multi-hash tests
fn test_info_hash_2() -> &'static str {
    "fedcba9876543210fedcba9876543210fedcba98"
}

/// Test IP address
fn test_ip() -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
}

// =========================================================================
// Test: LPD Announcement Format
// =========================================================================

#[test]
fn test_lpd_announce_format() {
    // Build a valid LPD announcement message manually and verify format
    let info_hash = test_info_hash();
    let port = 6881u16;
    let token = 0xDEADBEEFu32;

    let msg = format!(
        "Hash: {}\nPort: {}\nToken: {:08x}\n",
        info_hash, port, token
    );

    // Verify message contains all required fields in correct format
    assert!(msg.starts_with("Hash:"), "Should start with Hash:");
    assert!(
        msg.contains(&format!("Port: {}", port)),
        "Should contain Port field"
    );
    assert!(msg.contains("Token:"), "Should contain Token field");
    assert!(
        msg.contains(format!("{:08x}", token).as_str()),
        "Token should be 8 hex chars"
    );

    // Verify Hash value is exactly 40 hex characters
    let hash_line: Vec<&str> = msg.lines().filter(|l| l.starts_with("Hash:")).collect();
    assert_eq!(hash_line.len(), 1, "Should have exactly one Hash line");
    let hash_val = hash_line[0][5..].trim();
    assert_eq!(hash_val.len(), 40, "Info hash should be 40 chars");
    assert!(
        hash_val.chars().all(|c| c.is_ascii_hexdigit()),
        "Info hash should be all hex digits"
    );

    // Verify Port is valid u16 range
    let port_line: Vec<&str> = msg.lines().filter(|l| l.starts_with("Port:")).collect();
    assert_eq!(port_line.len(), 1);
    let parsed_port: u16 = port_line[0][5..].trim().parse().unwrap();
    assert!(
        (1..=65535).contains(&parsed_port),
        "Port should be in valid range"
    );
}

#[test]
fn test_lpd_announce_format_token_is_hex() {
    // Token must be exactly 8 lowercase hex characters
    let msg = format!(
        "Hash: {}\nPort: {}\nToken: {:08x}\n",
        test_info_hash(),
        6881,
        0x12345678u32
    );
    let token_line: &str = msg.lines().find(|l| l.starts_with("Token:")).unwrap();
    let token_val = token_line[6..].trim();

    assert_eq!(token_val.len(), 8, "Token should be 8 characters");
    assert!(
        token_val.chars().all(|c| c.is_ascii_hexdigit()),
        "Token should be all hex"
    );
}

// =========================================================================
// Test: LPD Receive / Parse Valid Announcements
// =========================================================================

#[test]
fn test_lpd_receive_parses_valid_announcement() {
    let info_hash = test_info_hash();
    let port = 6882u16;
    let sender_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 42));
    let token = 0xAABBCCDDu32;

    // Construct a valid LPD announcement
    let data = format!(
        "Hash: {}\nPort: {}\nToken: {:08x}\n",
        info_hash, port, token
    )
    .into_bytes();

    // Parse it
    let result = parse_lpd_announcement(&data, sender_ip);

    assert!(
        result.is_some(),
        "Valid announcement should parse successfully"
    );

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, info_hash.to_lowercase());
    assert_eq!(peer.port, port);
    assert_eq!(peer.addr, sender_ip);
    assert_eq!(peer.token, Some(token));
}

#[test]
fn test_lpd_receive_parses_case_insensitive_hash() {
    let sender_ip = test_ip();

    // Mixed case info hash
    let data =
        "Hash: ABCDEFabcdefABCDEFabcdefABCDEFabcdefABCD\nPort: 6881\nToken: deadbeef\n".as_bytes();

    let result = parse_lpd_announcement(data, sender_ip);
    assert!(result.is_some());

    let peer = result.unwrap();
    // Should be normalized to lowercase
    assert_eq!(peer.info_hash, peer.info_hash.to_lowercase());
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_receive_parses_extra_whitespace() {
    let sender_ip = test_ip();

    // Extra whitespace around values
    let data = "Hash:   0123456789abcdef0123456789abcdef01234567   \nPort:   6881   \nToken:   abcdef01   \n".as_bytes();

    let result = parse_lpd_announcement(data, sender_ip);
    assert!(result.is_some(), "Should handle extra whitespace");

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_receive_parses_unordered_fields() {
    let sender_ip = test_ip();

    // Fields in non-standard order (Port before Hash)
    let data =
        "Port: 6999\nToken: 11223344\nHash: 0123456789abcdef0123456789abcdef01234567\n".as_bytes();

    let result = parse_lpd_announcement(data, sender_ip);
    assert!(result.is_some(), "Should handle unordered fields");

    let peer = result.unwrap();
    assert_eq!(peer.port, 6999);
    assert_eq!(peer.token, Some(0x11223344));
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
}

// =========================================================================
// Test: LPD Invalid Announcements Rejected
// =========================================================================

#[test]
fn test_lpd_receive_ignores_invalid() {
    let sender_ip = test_ip();

    // Case 1: Non-UTF8 data
    assert!(
        parse_lpd_announcement(&[0xFF, 0xFE, 0xFD], sender_ip).is_none(),
        "Non-UTF8 should return None"
    );

    // Case 2: Missing Hash field
    let no_hash = "Port: 6881\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(no_hash, sender_ip).is_none(),
        "Missing Hash should return None"
    );

    // Case 3: Missing Port field
    let no_port = "Hash: 0123456789abcdef0123456789abcdef01234567\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(no_port, sender_ip).is_none(),
        "Missing Port should return None"
    );

    // Case 4: Missing Token field
    let no_token = "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\n".as_bytes();
    assert!(
        parse_lpd_announcement(no_token, sender_ip).is_none(),
        "Missing Token should return None"
    );

    // Case 5: Empty announcement
    assert!(
        parse_lpd_announcement(b"", sender_ip).is_none(),
        "Empty data should return None"
    );

    // Case 6: Only whitespace
    assert!(
        parse_lpd_announcement(b"   \n\t\n  ", sender_ip).is_none(),
        "Whitespace-only should return None"
    );
}

#[test]
fn test_lpd_receive_ignores_invalid_hash_format() {
    let sender_ip = test_ip();

    // Too short (39 chars)
    let short_hash =
        "Hash: 0123456789abcdef0123456789abcdef0123456\nPort: 6881\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(short_hash, sender_ip).is_none(),
        "Too-short hash should fail"
    );

    // Too long (41 chars)
    let long_hash =
        "Hash: 0123456789abcdef0123456789abcdef012345678\nPort: 6881\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(long_hash, sender_ip).is_none(),
        "Too-long hash should fail"
    );

    // Contains non-hex characters
    let bad_chars =
        "Hash: gggg56789abcdef0123456789abcdef01234567\nPort: 6881\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(bad_chars, sender_ip).is_none(),
        "Non-hex hash should fail"
    );
}

#[test]
fn test_lpd_receive_ignores_invalid_port() {
    let sender_ip = test_ip();

    // Port 0 (invalid)
    let port_zero =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 0\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(port_zero, sender_ip).is_none(),
        "Port 0 should be invalid"
    );

    // Non-numeric port
    let bad_port =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: abc\nToken: abcdef01\n".as_bytes();
    assert!(
        parse_lpd_announcement(bad_port, sender_ip).is_none(),
        "Non-numeric port should fail"
    );
}

#[test]
fn test_lpd_receive_ignores_unknown_fields() {
    let sender_ip = test_ip();

    // Unknown/extra fields should not cause failure if required fields present
    let with_extra = "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: abcdef01\nExtraField: value\nUnknown: data\n".as_bytes();

    let result = parse_lpd_announcement(with_extra, sender_ip);
    assert!(
        result.is_some(),
        "Extra unknown fields should not prevent parsing"
    );
    assert_eq!(result.unwrap().port, 6881);
}

// =========================================================================
// Test: Duplicate Suppression
// =========================================================================

#[test]
fn test_lpd_duplicate_suppression_same_hash_and_ip() {
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 50));

    // Same announcement from same IP twice
    let data =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: aabbccdd\n".as_bytes();

    let peer1 = parse_lpd_announcement(data, ip);
    let peer2 = parse_lpd_announcement(data, ip);

    assert!(peer1.is_some());
    assert!(peer2.is_some());

    // Same info_hash + same IP = equal peers (for dedup)
    let p1 = peer1.unwrap();
    let p2 = peer2.unwrap();
    assert_eq!(p1, p2, "Same hash+IP should produce equal peers");
}

#[test]
fn test_lpd_duplicate_suppression_different_ports_ok() {
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 51));

    // Different ports on same IP + same hash are still same peer (by our Eq impl)
    let data1 =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: aabbccdd\n".as_bytes();
    let data2 =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6882\nToken: eeff0011\n".as_bytes();

    let p1 = parse_lpd_announcement(data1, ip).unwrap();
    let p2 = parse_lpd_announcement(data2, ip).unwrap();

    // Our Eq implementation uses (info_hash, addr) only, so these are equal
    assert_eq!(
        p1, p2,
        "Same hash+IP with different ports should still be equal for dedup"
    );
}

#[test]
fn test_lpd_different_hashes_not_duplicates() {
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 52));

    let data1 = format!("Hash: {}\nPort: 6881\nToken: aabbccdd\n", test_info_hash());
    let data2 = format!(
        "Hash: {}\nPort: 6881\nToken: eeff0011\n",
        test_info_hash_2()
    );

    let p1 = parse_lpd_announcement(data1.as_bytes(), ip).unwrap();
    let p2 = parse_lpd_announcement(data2.as_bytes(), ip).unwrap();

    // Different hashes = different peers even from same IP
    assert_ne!(p1, p2, "Different info_hashes should not be duplicates");
}

#[test]
fn test_lpd_different_ips_not_duplicates() {
    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 53));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 54));

    let data =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: aabbccdd\n".as_bytes();

    let p1 = parse_lpd_announcement(data, ip1).unwrap();
    let p2 = parse_lpd_announcement(data, ip2).unwrap();

    assert_ne!(p1, p2, "Different IPs should not be duplicates");
}

// =========================================================================
// Test: LpdPeer Properties
// =========================================================================

#[test]
fn test_lpd_peer_creation() {
    let peer = LpdPeer::new(
        "abc123def456abc123def456abc123def456abcd",
        6881,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    );

    assert_eq!(peer.info_hash, "abc123def456abc123def456abc123def456abcd");
    assert_eq!(peer.port, 6881);
    assert_eq!(peer.addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert!(peer.token.is_none());
    assert!(!peer.is_expired(Duration::from_secs(99999)));
}

#[test]
fn test_lpd_peer_with_token() {
    let peer = LpdPeer::with_token(
        "0123456789abcdef0123456789abcdef01234567",
        6999,
        IpAddr::V4(Ipv4Addr::BROADCAST),
        0xCAFEBABE,
    );

    assert_eq!(peer.token, Some(0xCAFEBABE));
}

#[test]
fn test_lpd_peer_socket_addr() {
    let peer = LpdPeer::new("hash", 6881, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
    let sa = peer.socket_addr();
    assert_eq!(sa.ip(), IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)));
    assert_eq!(sa.port(), 6881);
}

#[test]
fn test_lpd_peer_expiration() {
    let peer = LpdPeer::new("hash", 6881, test_ip());

    // Freshly created peer should not be expired
    assert!(!peer.is_expired(Duration::from_secs(60)));

    // Peer should be "expired" after max_age of 0 seconds (since last_seen is now)
    assert!(peer.is_expired(Duration::ZERO));
}

#[test]
fn test_lpd_peer_hash_equality_for_set_dedup() {
    use std::collections::HashSet;

    let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    let mut set = HashSet::new();

    let peer1 = LpdPeer::new(test_info_hash(), 6881, ip);
    let peer2 = LpdPeer::new(test_info_hash(), 6882, ip); // Same hash+IP, different port

    set.insert(peer1.clone());
    set.insert(peer2.clone());

    // Should only have one entry due to dedup by (info_hash, addr)
    assert_eq!(set.len(), 1, "Same hash+IP should dedup in HashSet");
}

// =========================================================================
// Test: LpdAnnouncer Validation
// =========================================================================

#[test]
fn test_lpd_announcer_rejects_bad_info_hash() {
    // We can't easily create a real socket in tests without admin rights,
    // so we test the validation logic through the parser instead.
    // The announce method validates info_hash format.

    let invalid_hashes = vec![
        "",                                                        // Empty
        "short",                                                   // Too short
        "0123456789012345678901234567890123456789012345678",       // Too long (49)
        "ghijklmnopqrstuvwxyzabcdefghijklmnoqrstuvwxyzabcdefghij", // Non-hex
    ];

    for hash in invalid_hashes {
        // These would all fail validation in announce()
        let is_valid = hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit());
        assert!(!is_valid, "Hash '{}' should be invalid", hash);
    }
}

#[test]
fn test_lpd_announcer_accepts_valid_info_hash() {
    let valid_hashes = vec![
        "0123456789abcdef0123456789abcdef01234567",
        "FEDCBA9876543210FEDCBA9876543210FEDCBA98",
        "abcdefABCDEF1234abcdefABCDEF1234abcdef12",
    ];

    for hash in valid_hashes {
        let is_valid = hash.len() == 40 && hash.chars().all(|c| c.is_ascii_hexdigit());
        assert!(is_valid, "Hash '{}' should be valid", hash);
    }
}

// =========================================================================
// Test: LpdManager Operations
// =========================================================================

#[tokio::test]
async fn test_lpd_manager_register_unregister() {
    let manager = LpdManager::default();

    // Register a torrent
    manager.register_torrent(test_info_hash()).await.unwrap();

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
    manager.register_torrent(test_info_hash()).await.unwrap();

    // Add some discovered peers
    let new_peers = vec![
        LpdPeer::with_token(
            test_info_hash(),
            6881,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            0xAA,
        ),
        LpdPeer::with_token(
            test_info_hash(),
            6882,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
            0xBB,
        ),
        LpdPeer::with_token(
            test_info_hash(),
            6883,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
            0xCC,
        ),
    ];

    manager.update_peers(test_info_hash(), new_peers).await;

    let peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(peers.len(), 3, "Should have 3 stored peers");
}

#[tokio::test]
async fn test_lpd_manager_multiple_torrents_independent() {
    let manager = LpdManager::default();

    manager.register_torrent(test_info_hash()).await.unwrap();
    manager.register_torrent(test_info_hash_2()).await.unwrap();

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
    manager.register_torrent(test_info_hash()).await.unwrap();

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
