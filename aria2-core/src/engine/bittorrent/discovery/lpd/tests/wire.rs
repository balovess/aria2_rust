use super::*;

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
    assert_eq!(peer.info_hash.as_ref(), info_hash.to_lowercase());
    assert_eq!(peer.port, port);
    assert_eq!(peer.addr, sender_ip);
    assert!(peer.is_local, "10.x.x.x should be detected as local");
}

#[test]
fn test_lpd_receive_parses_bep14_case_insensitive_hash() {
    let sender_ip = test_ip();

    // Mixed case info hash in BEP 14 format
    let data = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: ABCDEFabcdefABCDEFabcdefABCDEFabcdefABCD\r\n\r\n\r\n".to_string().into_bytes();

    let result = parse_lpd_announcement(&data, sender_ip);
    assert!(result.is_some());

    let peer = result.unwrap();
    // Should be normalized to lowercase
    assert_eq!(peer.info_hash.as_ref(), peer.info_hash.to_lowercase());
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_receive_parses_bep14_extra_whitespace() {
    let sender_ip = test_ip();

    // Extra whitespace around values
    let data = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort:   6881   \r\nInfohash:   0123456789abcdef0123456789abcdef01234567   \r\n\r\n\r\n".to_string().into_bytes();

    let result = parse_lpd_announcement(&data, sender_ip);
    assert!(result.is_some(), "Should handle extra whitespace");

    let peer = result.unwrap();
    assert_eq!(
        peer.info_hash.as_ref(),
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(peer.port, 6881);
}

#[test]
fn test_lpd_receive_parses_bep14_unordered_headers() {
    let sender_ip = test_ip();

    // Infohash before Port (C++ uses HttpHeaderProcessor which is order-independent)
    let data = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\nPort: 6999\r\n\r\n\r\n".to_string().into_bytes();

    let result = parse_lpd_announcement(&data, sender_ip);
    assert!(result.is_some(), "Should handle unordered headers");

    let peer = result.unwrap();
    assert_eq!(peer.port, 6999);
    assert_eq!(
        peer.info_hash.as_ref(),
        "0123456789abcdef0123456789abcdef01234567"
    );
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
    assert_eq!(
        peer.info_hash.as_ref(),
        "0123456789abcdef0123456789abcdef01234567"
    );
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
    let short_hash = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef0123456\r\n\r\n\r\n".to_string().into_bytes();
    assert!(
        parse_lpd_announcement(&short_hash, sender_ip).is_none(),
        "Too-short hash should fail"
    );

    // Contains non-hex characters
    let bad_chars = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: gggg56789abcdef0123456789abcdef01234567\r\n\r\n\r\n".to_string().into_bytes();
    assert!(
        parse_lpd_announcement(&bad_chars, sender_ip).is_none(),
        "Non-hex hash should fail"
    );
}

#[test]
fn test_lpd_receive_ignores_invalid_port() {
    let sender_ip = test_ip();

    // Port 0 (invalid) in BEP14 format
    let port_zero = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 0\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n".to_string().into_bytes();
    assert!(
        parse_lpd_announcement(&port_zero, sender_ip).is_none(),
        "Port 0 should be invalid"
    );

    // Non-numeric port in BEP14 format
    let bad_port = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: abc\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n".to_string().into_bytes();
    assert!(
        parse_lpd_announcement(&bad_port, sender_ip).is_none(),
        "Non-numeric port should fail"
    );
}

#[test]
fn test_lpd_receive_ignores_unknown_fields() {
    let sender_ip = test_ip();

    // Unknown/extra headers should not cause failure
    let with_extra = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\nExtraField: value\r\nUnknown: data\r\n\r\n\r\n".to_string().into_bytes();

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
