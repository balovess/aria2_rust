//! Tests for Local Peer Discovery (LPD) Manager - Phase 15 H8
//!
//! Comprehensive tests covering:
//! - BEP 14 LPD announcement format validation
//! - Announcement parsing (valid, invalid, edge cases)
//! - Legacy format backward compatibility
//! - Duplicate suppression
//! - LpdPeer equality and hashing
//! - LpdManager lifecycle operations
//! - Private address detection

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use super::{LpdAnnouncer, LpdManager, LpdPeer, is_private_address, parse_lpd_announcement};

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

/// Build a BEP 14 compliant LPD message
fn make_bep14_message(info_hash: &str, port: u16) -> Vec<u8> {
    format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: {}\r\nInfohash: {}\r\n\r\n\r\n",
        port, info_hash
    )
    .into_bytes()
}

// =========================================================================
// Test: Multicast interface configuration
// =========================================================================

#[test]
fn test_lpd_announcer_exposes_configured_interface() {
    let announcer = LpdAnnouncer::with_interface(30, Some(Ipv4Addr::LOCALHOST)).unwrap();
    assert_eq!(announcer.interface(), Some(Ipv4Addr::LOCALHOST));
}

// =========================================================================
// Test: BEP 14 Announcement Format
// =========================================================================

#[test]
fn test_lpd_bep14_format() {
    // Build a valid BEP 14 LPD announcement and verify format
    let info_hash = test_info_hash();
    let port = 6881u16;

    let msg = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: {}\r\nInfohash: {}\r\n\r\n\r\n",
        port, info_hash
    );

    // Verify message contains BEP 14 request line
    assert!(
        msg.starts_with("BT-SEARCH * HTTP/1.1\r\n"),
        "Should start with BEP 14 request line"
    );
    assert!(
        msg.contains("Host: 239.192.152.143:6771"),
        "Should contain Host header"
    );
    assert!(
        msg.contains(&format!("Port: {}", port)),
        "Should contain Port header"
    );
    assert!(
        msg.contains(&format!("Infohash: {}", info_hash)),
        "Should contain Infohash header"
    );
    assert!(msg.ends_with("\r\n\r\n\r\n"), "Should end with double CRLF");
}

// =========================================================================
// Test: LPD Receive / Parse BEP 14 Announcements
// =========================================================================

#[test]
fn test_lpd_receive_parses_bep14_announcement() {
    let info_hash = test_info_hash();
    let port = 6882u16;
    let sender_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 42));

    let data = make_bep14_message(info_hash, port);
    let result = parse_lpd_announcement(&data, sender_ip);

    assert!(
        result.is_some(),
        "Valid BEP 14 announcement should parse successfully"
    );

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, info_hash.to_lowercase());
    assert_eq!(peer.port, port);
    assert_eq!(peer.addr, sender_ip);
    assert!(peer.is_local, "10.x.x.x should be detected as local");
}

#[test]
fn test_lpd_receive_parses_bep14_case_insensitive_hash() {
    let sender_ip = test_ip();

    // Mixed case info hash in BEP 14 format
    let data = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: ABCDEFabcdefABCDEFabcdefABCDEFabcdefABCD\r\n\r\n\r\n"
    ).into_bytes();

    let result = parse_lpd_announcement(&data, sender_ip);
    assert!(result.is_some());

    let peer = result.unwrap();
    // Should be normalized to lowercase
    assert_eq!(peer.info_hash, peer.info_hash.to_lowercase());
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_receive_parses_bep14_extra_whitespace() {
    let sender_ip = test_ip();

    // Extra whitespace around values
    let data = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort:   6881   \r\nInfohash:   0123456789abcdef0123456789abcdef01234567   \r\n\r\n\r\n"
    ).into_bytes();

    let result = parse_lpd_announcement(&data, sender_ip);
    assert!(result.is_some(), "Should handle extra whitespace");

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_receive_parses_bep14_unordered_headers() {
    let sender_ip = test_ip();

    // Infohash before Port (C++ uses HttpHeaderProcessor which is order-independent)
    let data = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\nPort: 6999\r\n\r\n\r\n"
    ).into_bytes();

    let result = parse_lpd_announcement(&data, sender_ip);
    assert!(result.is_some(), "Should handle unordered headers");

    let peer = result.unwrap();
    assert_eq!(peer.port, 6999);
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
}

// =========================================================================
// Test: Legacy Format Backward Compatibility
// =========================================================================

#[test]
fn test_lpd_receive_parses_legacy_format() {
    let sender_ip = test_ip();

    // Old proprietary format (Hash:/Port:/Token:) should still parse
    let data =
        "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: abcdef01\n".as_bytes();

    let result = parse_lpd_announcement(data, sender_ip);
    assert!(
        result.is_some(),
        "Legacy format should be parsed for backward compatibility"
    );

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_legacy_format_without_token_still_works() {
    let sender_ip = test_ip();

    // Legacy format without Token field should also parse
    let data = "Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\n".as_bytes();

    let result = parse_lpd_announcement(data, sender_ip);
    assert!(result.is_some(), "Legacy format without Token should parse");

    let peer = result.unwrap();
    assert_eq!(peer.port, 6881);
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

    // Case 2: Missing Infohash field in BEP14 format
    let no_infohash =
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\n\r\n\r\n".as_bytes();
    assert!(
        parse_lpd_announcement(no_infohash, sender_ip).is_none(),
        "Missing Infohash should return None"
    );

    // Case 3: Missing Port field in BEP14 format
    let no_port = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n".as_bytes();
    assert!(
        parse_lpd_announcement(no_port, sender_ip).is_none(),
        "Missing Port should return None"
    );

    // Case 4: Empty announcement
    assert!(
        parse_lpd_announcement(b"", sender_ip).is_none(),
        "Empty data should return None"
    );

    // Case 5: Only whitespace
    assert!(
        parse_lpd_announcement(b"   \n\t\n  ", sender_ip).is_none(),
        "Whitespace-only should return None"
    );
}

#[test]
fn test_lpd_receive_ignores_invalid_hash_format() {
    let sender_ip = test_ip();

    // Too short (39 chars) in BEP14 format
    let short_hash = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef0123456\r\n\r\n\r\n"
    ).into_bytes();
    assert!(
        parse_lpd_announcement(&short_hash, sender_ip).is_none(),
        "Too-short hash should fail"
    );

    // Contains non-hex characters
    let bad_chars = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: gggg56789abcdef0123456789abcdef01234567\r\n\r\n\r\n"
    ).into_bytes();
    assert!(
        parse_lpd_announcement(&bad_chars, sender_ip).is_none(),
        "Non-hex hash should fail"
    );
}

#[test]
fn test_lpd_receive_ignores_invalid_port() {
    let sender_ip = test_ip();

    // Port 0 (invalid) in BEP14 format
    let port_zero = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 0\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n"
    ).into_bytes();
    assert!(
        parse_lpd_announcement(&port_zero, sender_ip).is_none(),
        "Port 0 should be invalid"
    );

    // Non-numeric port in BEP14 format
    let bad_port = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: abc\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n"
    ).into_bytes();
    assert!(
        parse_lpd_announcement(&bad_port, sender_ip).is_none(),
        "Non-numeric port should fail"
    );
}

#[test]
fn test_lpd_receive_ignores_unknown_fields() {
    let sender_ip = test_ip();

    // Unknown/extra headers should not cause failure
    let with_extra = format!(
        "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\nExtraField: value\r\nUnknown: data\r\n\r\n\r\n"
    ).into_bytes();

    let result = parse_lpd_announcement(&with_extra, sender_ip);
    assert!(
        result.is_some(),
        "Extra unknown headers should not prevent parsing"
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
    let data = make_bep14_message("0123456789abcdef0123456789abcdef01234567", 6881);

    let peer1 = parse_lpd_announcement(&data, ip);
    let peer2 = parse_lpd_announcement(&data, ip);

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

    let data1 = make_bep14_message("0123456789abcdef0123456789abcdef01234567", 6881);
    let data2 = make_bep14_message("0123456789abcdef0123456789abcdef01234567", 6882);

    let p1 = parse_lpd_announcement(&data1, ip).unwrap();
    let p2 = parse_lpd_announcement(&data2, ip).unwrap();

    // Our Eq implementation uses (info_hash, addr) only, so these are equal
    assert_eq!(
        p1, p2,
        "Same hash+IP with different ports should still be equal for dedup"
    );
}

#[test]
fn test_lpd_different_hashes_not_duplicates() {
    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 52));

    let data1 = make_bep14_message(test_info_hash(), 6881);
    let data2 = make_bep14_message(test_info_hash_2(), 6881);

    let p1 = parse_lpd_announcement(&data1, ip).unwrap();
    let p2 = parse_lpd_announcement(&data2, ip).unwrap();

    // Different hashes = different peers even from same IP
    assert_ne!(p1, p2, "Different info_hashes should not be duplicates");
}

#[test]
fn test_lpd_different_ips_not_duplicates() {
    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 53));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 54));

    let data = make_bep14_message("0123456789abcdef0123456789abcdef01234567", 6881);

    let p1 = parse_lpd_announcement(&data, ip1).unwrap();
    let p2 = parse_lpd_announcement(&data, ip2).unwrap();

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
    assert!(peer.is_local, "127.0.0.1 should be detected as local");
    assert!(!peer.is_expired(Duration::from_secs(99999)));
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
// Test: Private Address Detection
// =========================================================================

#[test]
fn test_is_private_address_ipv4() {
    // 10.0.0.0/8
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1))));
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        10, 255, 255, 255
    ))));

    // 172.16.0.0/12
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        172, 16, 0, 1
    ))));
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        172, 31, 255, 255
    ))));

    // 192.168.0.0/16
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        192, 168, 0, 1
    ))));
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        192, 168, 255, 255
    ))));

    // 127.0.0.0/8 (loopback)
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::LOCALHOST)));

    // 169.254.0.0/16 (link-local)
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        169, 254, 0, 1
    ))));
    assert!(is_private_address(&IpAddr::V4(Ipv4Addr::new(
        169, 254, 255, 255
    ))));

    // NOT private
    assert!(!is_private_address(&IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
    assert!(!is_private_address(&IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))));
    assert!(!is_private_address(&IpAddr::V4(Ipv4Addr::new(
        172, 15, 0, 1
    ))));
    assert!(!is_private_address(&IpAddr::V4(Ipv4Addr::new(
        172, 32, 0, 1
    ))));
}

#[test]
fn test_is_private_address_ipv6() {
    // fc00::/7 (unique local)
    assert!(is_private_address(&IpAddr::V6(Ipv6Addr::new(
        0xfc00, 0, 0, 0, 0, 0, 0, 1
    ))));
    assert!(is_private_address(&IpAddr::V6(Ipv6Addr::new(
        0xfdff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff, 0xffff
    ))));

    // ::1 (loopback)
    assert!(is_private_address(&IpAddr::V6(Ipv6Addr::LOCALHOST)));

    // NOT private
    assert!(!is_private_address(&IpAddr::V6(Ipv6Addr::new(
        0x2001, 0xdb8, 0, 0, 0, 0, 0, 1
    ))));
}

// =========================================================================
// Test: LpdAnnouncer Validation
// =========================================================================

#[test]
fn test_lpd_announcer_rejects_bad_info_hash() {
    let invalid_hashes = vec![
        "",                                                        // Empty
        "short",                                                   // Too short
        "0123456789012345678901234567890123456789012345678",       // Too long (49)
        "ghijklmnopqrstuvwxyzabcdefghijklmnoqrstuvwxyzabcdefghij", // Non-hex
    ];

    for hash in invalid_hashes {
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
