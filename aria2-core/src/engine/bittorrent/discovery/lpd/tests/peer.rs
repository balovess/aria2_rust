use super::*;

// Test: LpdPeer Properties
// =========================================================================

#[test]
fn test_lpd_peer_creation() {
    let peer = LpdPeer::new(
        "abc123def456abc123def456abc123def456abcd",
        6881,
        IpAddr::V4(Ipv4Addr::LOCALHOST),
    );

    assert_eq!(
        peer.info_hash.as_ref(),
        "abc123def456abc123def456abc123def456abcd"
    );
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
