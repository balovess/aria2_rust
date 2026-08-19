//! E2E tests for Local Peer Discovery (LPD) functionality
//!
//! Tests cover:
//! 1. LPD announcement format validation (BEP 14)
//! 2. LPD peer discovery with mock UDP
//! 3. LPD integration with BitTorrent download
//!
//! LPD Protocol (BEP-14):
//! - Multicast Group: 239.192.152.143:6771
//! - Message Format: BT-SEARCH * HTTP/1.1\r\nHost: ...\r\nPort: X\r\nInfohash: X\r\n\r\n\r\n

#![cfg(feature = "bittorrent")]

mod fixtures;
use fixtures::mock_lpd_peer::{
    MockLpdPeer, MockLpdServer, build_bep14_announcement, make_test_info_hash,
};

use std::net::{IpAddr, Ipv4Addr, UdpSocket};
use std::time::Duration;

use aria2_core::engine::lpd_manager::{
    DEFAULT_ANNOUNCE_INTERVAL_SECS, LPD_MULTICAST_ADDR, LPD_PORT, LpdManager, LpdPeer,
    parse_lpd_announcement,
};

// =========================================================================
// Test Helpers
// =========================================================================

/// Create a valid 40-char hex info hash for testing
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
// Section 1: LPD Announcement Format Tests
// =========================================================================

/// Test that LPD announcement format follows BEP-14 specification
#[test]
fn test_lpd_announcement_format_spec_compliance() {
    let info_hash = test_info_hash();
    let port = 6881u16;

    // Build announcement per BEP-14 spec
    let msg = build_bep14_announcement(info_hash, port);

    // Verify format compliance
    // 1. Must start with BT-SEARCH request line
    assert!(
        msg.starts_with("BT-SEARCH * HTTP/1.1\r\n"),
        "Announcement must start with BT-SEARCH request line"
    );

    // 2. Host header must be present with multicast address
    assert!(
        msg.contains("Host: 239.192.152.143:6771\r\n"),
        "Host header must specify multicast group"
    );

    // 3. Infohash field must be present with 40-char hex value
    let infohash_val = msg
        .lines()
        .find(|l| l.starts_with("Infohash:"))
        .map(|l| l.strip_prefix("Infohash:").unwrap().trim())
        .unwrap();
    assert_eq!(
        infohash_val.len(),
        40,
        "Info hash must be exactly 40 characters"
    );
    assert!(
        infohash_val.chars().all(|c| c.is_ascii_hexdigit()),
        "Info hash must be all hex digits"
    );

    // 4. Port field must be present with valid port number
    let port_val = msg
        .lines()
        .find(|l| l.starts_with("Port:"))
        .map(|l| l.strip_prefix("Port:").unwrap().trim())
        .unwrap();
    let parsed_port: u16 = port_val.parse().unwrap();
    assert!(parsed_port > 0, "Port must be valid");

    // 5. Message must end with double CRLF
    assert!(
        msg.ends_with("\r\n\r\n"),
        "Announcement must end with double CRLF"
    );
}

/// Test announcement format with uppercase hash (should be normalized)
#[test]
fn test_lpd_announcement_format_case_normalization() {
    let uppercase_hash = "ABCDEF1234567890ABCDEF1234567890ABCDEF12";
    let msg = build_bep14_announcement(uppercase_hash, 6881);

    // Parser should normalize to lowercase
    let result = parse_lpd_announcement(msg.as_bytes(), test_ip());
    assert!(result.is_some());

    let peer = result.unwrap();
    // Hash should be normalized to lowercase
    assert_eq!(peer.info_hash, uppercase_hash.to_lowercase());
}

/// Test announcement format with extra whitespace around values (should be trimmed)
#[test]
fn test_lpd_announcement_format_whitespace_handling() {
    let msg = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort:   6881   \r\nInfohash:   0123456789abcdef0123456789abcdef01234567   \r\n\r\n\r\n";

    let result = parse_lpd_announcement(msg.as_bytes(), test_ip());
    assert!(result.is_some(), "Should handle extra whitespace");

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(peer.port, 6881);
}

/// Test announcement format with unknown fields (should be ignored)
#[test]
fn test_lpd_announcement_format_unknown_fields_ignored() {
    let msg = "BT-SEARCH * HTTP/1.1\r\nHost: 239.192.152.143:6771\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\nUnknownField: value\r\nVersion: 1.0\r\n\r\n\r\n";

    let result = parse_lpd_announcement(msg.as_bytes(), test_ip());
    assert!(
        result.is_some(),
        "Unknown fields should not prevent parsing"
    );

    let peer = result.unwrap();
    assert_eq!(peer.port, 6881);
}

/// Test invalid announcement formats are rejected
#[test]
fn test_lpd_announcement_format_invalid_rejected() {
    let sender_ip = test_ip();

    // Missing Infohash (only Port)
    assert!(
        parse_lpd_announcement(b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\n\r\n\r\n", sender_ip)
            .is_none()
    );

    // Missing Port (only Infohash)
    assert!(
        parse_lpd_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n",
            sender_ip
        )
        .is_none()
    );

    // Invalid hash length (39 chars)
    assert!(
        parse_lpd_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef0123456\r\n\r\n\r\n",
            sender_ip
        )
        .is_none()
    );

    // Invalid hash length (41 chars)
    assert!(
        parse_lpd_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: 0123456789abcdef0123456789abcdef012345678\r\n\r\n\r\n",
            sender_ip
        )
        .is_none()
    );

    // Non-hex characters in hash
    assert!(
        parse_lpd_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nPort: 6881\r\nInfohash: ghijklmnopqrstuvwxyzabcdefghijklmnopqrstu\r\n\r\n\r\n",
            sender_ip
        )
        .is_none()
    );

    // Port 0 (invalid)
    assert!(
        parse_lpd_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nPort: 0\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n",
            sender_ip
        )
        .is_none()
    );

    // Non-numeric port
    assert!(
        parse_lpd_announcement(
            b"BT-SEARCH * HTTP/1.1\r\nPort: abc\r\nInfohash: 0123456789abcdef0123456789abcdef01234567\r\n\r\n\r\n",
            sender_ip
        )
        .is_none()
    );

    // Empty announcement
    assert!(parse_lpd_announcement(b"", sender_ip).is_none());

    // Non-UTF8 data
    assert!(parse_lpd_announcement(&[0xFF, 0xFE, 0xFD], sender_ip).is_none());
}

/// Test that legacy format (Hash:/Port:/Token:) is still accepted
#[test]
fn test_lpd_announcement_legacy_format_accepted() {
    let sender_ip = test_ip();

    // Legacy format with Token should still parse
    let result = parse_lpd_announcement(
        b"Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\nToken: abcdef01\n",
        sender_ip,
    );
    assert!(
        result.is_some(),
        "Legacy format should be accepted for backward compat"
    );

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(peer.port, 6881);

    // Legacy format without Token should also parse
    let result2 = parse_lpd_announcement(
        b"Hash: 0123456789abcdef0123456789abcdef01234567\nPort: 6881\n",
        sender_ip,
    );
    assert!(
        result2.is_some(),
        "Legacy format without Token should be accepted"
    );
}

/// Test LPD constants are correct per BEP-14
#[test]
fn test_lpd_constants_spec_compliance() {
    // Multicast address per BEP-14
    assert_eq!(LPD_MULTICAST_ADDR, "239.192.152.143");

    // Port per BEP-14
    assert_eq!(LPD_PORT, 6771);

    // Default interval (5 minutes per BEP-14 recommendation)
    assert_eq!(DEFAULT_ANNOUNCE_INTERVAL_SECS, 300);
}

// =========================================================================
// Section 2: LPD Peer Discovery with Mock UDP
// =========================================================================

/// Test basic mock LPD server functionality
#[test]
fn test_mock_lpd_server_basic() {
    let server = MockLpdServer::new().expect("Failed to create mock LPD server");

    // Server should have valid port
    assert!(server.port() > 0);

    // Server address should be valid
    let addr = server.addr();
    assert!(addr.ip().is_loopback() || addr.ip() == IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
}

/// Test mock LPD peer announcement formatting
#[test]
fn test_mock_lpd_peer_announcement() {
    let peer = MockLpdPeer::new(test_info_hash(), 6881, Ipv4Addr::new(192, 168, 1, 50));

    let msg = peer.format_announcement();

    // Verify BEP 14 message format
    assert!(msg.starts_with("BT-SEARCH * HTTP/1.1\r\n"));
    assert!(msg.contains(test_info_hash()));
    assert!(msg.contains("6881"));

    // Parse it back
    let result = parse_lpd_announcement(msg.as_bytes(), test_ip());
    assert!(result.is_some());
}

/// Test sending LPD announcement between mock peers
#[test]
fn test_lpd_send_announcement_between_peers() {
    // Create two sockets for peer-to-peer communication
    let socket1 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket1");
    let socket2 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket2");

    socket1
        .set_broadcast(true)
        .expect("Failed to enable broadcast");
    socket2
        .set_broadcast(true)
        .expect("Failed to enable broadcast");

    let _addr1 = socket1.local_addr().expect("Failed to get addr1");
    let addr2 = socket2.local_addr().expect("Failed to get addr2");

    // Peer1 sends announcement to Peer2
    let peer1 = MockLpdPeer::new(test_info_hash(), 6881, Ipv4Addr::new(127, 0, 0, 1));
    peer1
        .announce_to(&socket1, addr2)
        .expect("Failed to announce");

    // Peer2 receives announcement
    socket2
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("Failed to set timeout");
    let mut buf = [0u8; 1024];

    match socket2.recv_from(&mut buf) {
        Ok((len, src_addr)) => {
            let result = parse_lpd_announcement(&buf[..len], src_addr.ip());
            assert!(result.is_some(), "Should receive valid announcement");

            let peer = result.unwrap();
            assert_eq!(peer.info_hash, test_info_hash());
            assert_eq!(peer.port, 6881);
        }
        Err(e) => {
            panic!("Failed to receive announcement: {}", e);
        }
    }
}

/// Test LPD peer discovery simulation
#[test]
fn test_lpd_peer_discovery_simulation() {
    // Create mock server
    let server = MockLpdServer::new().expect("Failed to create mock server");
    server.start();

    // Wait for server to start
    std::thread::sleep(Duration::from_millis(100));

    // Create client socket
    let client_socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind client socket");
    client_socket
        .set_broadcast(true)
        .expect("Failed to enable broadcast");

    // Send announcement to server
    let peer = MockLpdPeer::new(test_info_hash(), 6881, Ipv4Addr::new(127, 0, 0, 1));
    peer.announce_to(&client_socket, server.addr())
        .expect("Failed to send announcement");

    // Wait for processing
    std::thread::sleep(Duration::from_millis(200));

    // Check server received announcement
    let received = server.get_received();
    assert!(
        !received.is_empty(),
        "Server should have received announcement"
    );

    let first_ann = &received[0];
    assert_eq!(first_ann.info_hash, test_info_hash());
    assert_eq!(first_ann.port, 6881);

    server.stop();
}

/// Test LPD multiple peer discovery
#[test]
fn test_lpd_multiple_peer_discovery() {
    let server = MockLpdServer::new().expect("Failed to create mock server");
    server.start();

    std::thread::sleep(Duration::from_millis(100));

    // Create multiple client sockets
    let socket1 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket1");
    let socket2 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket2");
    let socket3 = UdpSocket::bind("127.0.0.1:0").expect("Failed to bind socket3");

    for s in [&socket1, &socket2, &socket3] {
        s.set_broadcast(true).expect("Failed to enable broadcast");
    }

    // Send announcements from different peers
    let peer1 = MockLpdPeer::new(test_info_hash(), 6881, Ipv4Addr::new(127, 0, 0, 1));
    let peer2 = MockLpdPeer::new(test_info_hash(), 6882, Ipv4Addr::new(127, 0, 0, 2));
    let peer3 = MockLpdPeer::new(test_info_hash_2(), 6883, Ipv4Addr::new(127, 0, 0, 3));

    peer1
        .announce_to(&socket1, server.addr())
        .expect("Failed to announce peer1");
    peer2
        .announce_to(&socket2, server.addr())
        .expect("Failed to announce peer2");
    peer3
        .announce_to(&socket3, server.addr())
        .expect("Failed to announce peer3");

    // Wait for processing
    std::thread::sleep(Duration::from_millis(300));

    // Check server received all announcements
    let received = server.get_received();
    assert!(
        received.len() >= 3,
        "Server should have received at least 3 announcements"
    );

    // Check unique hashes
    let unique_hashes = server.unique_hashes();
    assert_eq!(unique_hashes.len(), 2, "Should have 2 unique info hashes");

    server.stop();
}

/// Test LPD peer registration and discovery
#[tokio::test]
async fn test_lpd_peer_registration_and_discovery() {
    // Create LPD manager
    let manager = LpdManager::default();

    // Register a torrent
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register torrent");

    // Check active hashes
    let active = manager.active_hashes.read().await;
    assert!(
        active.contains(test_info_hash()),
        "Torrent should be registered"
    );
    drop(active);

    // Create mock peers
    let peer1 = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
    );
    let peer2 = LpdPeer::new(
        test_info_hash(),
        6882,
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)),
    );

    // Update peers
    manager
        .update_peers(test_info_hash(), vec![peer1.clone(), peer2.clone()])
        .await;

    // Get peers for torrent
    let peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(peers.len(), 2, "Should have 2 registered peers");

    // Verify peer details
    let found_peer1 = peers.iter().find(|p| p.port == 6881);
    let found_peer2 = peers.iter().find(|p| p.port == 6882);

    assert!(found_peer1.is_some(), "Should find peer with port 6881");
    assert!(found_peer2.is_some(), "Should find peer with port 6882");

    // Unregister torrent
    manager.unregister_torrent(test_info_hash()).await;

    let active = manager.active_hashes.read().await;
    assert!(
        !active.contains(test_info_hash()),
        "Torrent should be unregistered"
    );
}

/// Test LPD peer deduplication
#[tokio::test]
async fn test_lpd_peer_deduplication() {
    let manager = LpdManager::default();
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register");

    // Add same peer multiple times (same info_hash + IP)
    let peer = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
    );

    // Add peer three times
    manager
        .update_peers(test_info_hash(), vec![peer.clone()])
        .await;
    manager
        .update_peers(test_info_hash(), vec![peer.clone()])
        .await;
    manager
        .update_peers(test_info_hash(), vec![peer.clone()])
        .await;

    // Should only have one peer due to deduplication
    let peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(
        peers.len(),
        1,
        "Should deduplicate peers by (info_hash, addr)"
    );
}

/// Test LPD peer expiration cleanup
#[tokio::test]
async fn test_lpd_peer_expiration_cleanup() {
    let manager = LpdManager::default();
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register");

    // Add a peer
    let peer = LpdPeer::new(test_info_hash(), 6881, test_ip());
    manager.update_peers(test_info_hash(), vec![peer]).await;

    // Verify peer exists
    let peers_before = manager.get_peers_for(test_info_hash()).await;
    assert!(!peers_before.is_empty(), "Should have peer before cleanup");

    // Cleanup with zero tolerance (all peers expired)
    let removed = manager.cleanup_expired_peers(Duration::ZERO).await;
    assert!(removed > 0, "Should remove expired peers");

    // Verify peer removed
    let peers_after = manager.get_peers_for(test_info_hash()).await;
    assert!(peers_after.is_empty(), "Should have no peers after cleanup");
}

/// Test LPD manager availability check
#[test]
fn test_lpd_manager_availability() {
    let manager = LpdManager::default();
    assert!(
        manager.is_available(),
        "Default LPD manager should be available"
    );
}

// =========================================================================
// Section 3: LPD Integration with BitTorrent Download
// =========================================================================

/// Test LPD announcement during torrent registration
#[tokio::test]
async fn test_lpd_torrent_registration_announcement() {
    let manager = LpdManager::default();

    // Register multiple torrents
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register hash1");
    manager
        .register_torrent(test_info_hash_2(), false)
        .await
        .expect("Failed to register hash2");

    // Check both are registered
    let active = manager.active_hashes.read().await;
    assert!(active.contains(test_info_hash()));
    assert!(active.contains(test_info_hash_2()));
    assert_eq!(active.len(), 2);
}

/// Test LPD peer discovery integration
#[tokio::test]
async fn test_lpd_peer_discovery_integration() {
    let manager = LpdManager::default();
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register");

    // Simulate discovering peers via LPD
    let discovered_peers = vec![
        LpdPeer::new(
            test_info_hash(),
            6881,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)),
        ),
        LpdPeer::new(
            test_info_hash(),
            6882,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 11)),
        ),
        LpdPeer::new(
            test_info_hash(),
            6883,
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 12)),
        ),
    ];

    // Update peers from discovery
    manager
        .update_peers(test_info_hash(), discovered_peers)
        .await;

    // Verify peers are stored
    let stored_peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(stored_peers.len(), 3, "Should have 3 discovered peers");

    // Verify peer addresses are correct
    for peer in &stored_peers {
        assert!(peer.addr.is_ipv4(), "Peer address should be IPv4");
        assert!(peer.port > 0, "Peer port should be valid");
    }
}

/// Test LPD with multiple torrents independent peer tracking
#[tokio::test]
async fn test_lpd_multiple_torrents_independent_tracking() {
    let manager = LpdManager::default();

    // Register two torrents
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register hash1");
    manager
        .register_torrent(test_info_hash_2(), false)
        .await
        .expect("Failed to register hash2");

    // Add peers to each torrent
    let peers1 = vec![
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
    ];

    let peers2 = vec![
        LpdPeer::new(
            test_info_hash_2(),
            6991,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
        ),
        LpdPeer::new(
            test_info_hash_2(),
            6992,
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 4)),
        ),
    ];

    manager.update_peers(test_info_hash(), peers1).await;
    manager.update_peers(test_info_hash_2(), peers2).await;

    // Verify peers are tracked independently
    let stored1 = manager.get_peers_for(test_info_hash()).await;
    let stored2 = manager.get_peers_for(test_info_hash_2()).await;

    assert_eq!(stored1.len(), 2, "Torrent 1 should have 2 peers");
    assert_eq!(stored2.len(), 2, "Torrent 2 should have 2 peers");

    // Verify peers belong to correct torrents
    for peer in &stored1 {
        assert_eq!(peer.info_hash, test_info_hash());
    }
    for peer in &stored2 {
        assert_eq!(peer.info_hash, test_info_hash_2());
    }
}

/// Test LPD peer socket address generation
#[test]
fn test_lpd_peer_socket_addr() {
    let peer = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)),
    );

    let socket_addr = peer.socket_addr();

    assert_eq!(
        socket_addr.ip(),
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100))
    );
    assert_eq!(socket_addr.port(), 6881);
}

/// Test LPD announcement message building
#[test]
fn test_lpd_announcement_message_building() {
    let info_hash = make_test_info_hash(0x42);
    let port = 6881u16;

    // Build BEP 14 announcement message
    let msg = build_bep14_announcement(&info_hash, port);

    // Parse it back
    let result = parse_lpd_announcement(msg.as_bytes(), test_ip());
    assert!(
        result.is_some(),
        "Built announcement should parse successfully"
    );

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, info_hash);
    assert_eq!(peer.port, port);
}

/// Test LPD peer equality for deduplication
#[test]
fn test_lpd_peer_equality_for_dedup() {
    use std::collections::HashSet;

    let ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));

    // Same info_hash + same IP = equal peers
    let peer1 = LpdPeer::new(test_info_hash(), 6881, ip);
    let peer2 = LpdPeer::new(test_info_hash(), 6882, ip); // Different port, same hash+IP

    assert_eq!(peer1, peer2, "Same hash+IP should be equal for dedup");

    // Test HashSet deduplication
    let mut set = HashSet::new();
    set.insert(peer1);
    set.insert(peer2);
    assert_eq!(set.len(), 1, "HashSet should deduplicate by (hash, IP)");
}

/// Test LPD peer hash consistency
#[test]
fn test_lpd_peer_hash_consistency() {
    use std::collections::HashSet;

    let ip1 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
    let ip2 = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));

    // Different IPs should produce different hashes
    let peer1 = LpdPeer::new(test_info_hash(), 6881, ip1);
    let peer2 = LpdPeer::new(test_info_hash(), 6881, ip2);

    assert_ne!(peer1, peer2, "Different IPs should not be equal");

    // Both should be added to HashSet
    let mut set = HashSet::new();
    set.insert(peer1);
    set.insert(peer2);
    assert_eq!(set.len(), 2, "Different IPs should not deduplicate");
}

/// Test LPD with mock BitTorrent peer simulation
#[tokio::test]
async fn test_lpd_bittorrent_peer_simulation() {
    // This test simulates LPD discovery of a BitTorrent peer

    // Create LPD manager
    let manager = LpdManager::default();

    // Register torrent
    let info_hash = make_test_info_hash(0xAB);
    manager
        .register_torrent(&info_hash, false)
        .await
        .expect("Failed to register");

    // Simulate LPD discovering a peer that has the torrent
    let discovered_peer =
        LpdPeer::new(&info_hash, 6881, IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50)));

    // Update peer registry
    manager
        .update_peers(&info_hash, vec![discovered_peer.clone()])
        .await;

    // Verify peer is available for connection
    let peers = manager.get_peers_for(&info_hash).await;
    assert!(!peers.is_empty(), "Should have discovered peer");

    let peer = &peers[0];
    let peer_addr = peer.socket_addr();

    // Verify peer address is usable for BitTorrent connection
    assert!(
        peer_addr.port() > 0,
        "Peer port should be valid for BT connection"
    );
    assert!(peer_addr.ip().is_ipv4(), "Peer IP should be IPv4 for LPD");

    // Cleanup
    manager.unregister_torrent(&info_hash).await;
}

/// Test LPD peer max limit enforcement
#[tokio::test]
async fn test_lpd_peer_max_limit() {
    use aria2_core::engine::lpd_manager::MAX_PEERS_PER_HASH;

    let manager = LpdManager::default();
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register");

    // Add more peers than the max limit
    let mut many_peers = Vec::new();
    for i in 0..(MAX_PEERS_PER_HASH + 10) {
        let peer = LpdPeer::new(
            test_info_hash(),
            6881 + (i as u16),
            IpAddr::V4(Ipv4Addr::new(10, 0, i as u8, 1)),
        );
        many_peers.push(peer);
    }

    // Update peers (should enforce limit)
    manager.update_peers(test_info_hash(), many_peers).await;

    // Check that peer count is limited
    let stored_peers = manager.get_peers_for(test_info_hash()).await;
    assert!(
        stored_peers.len() <= MAX_PEERS_PER_HASH,
        "Peer count should not exceed MAX_PEERS_PER_HASH ({})",
        MAX_PEERS_PER_HASH
    );
}

/// Test LPD announcement interval configuration
#[test]
fn test_lpd_announcement_interval_config() {
    // Test with custom interval
    let manager = LpdManager::with_interval(60).expect("Failed to create manager with interval");

    // Manager should be available
    assert!(manager.is_available());
}

/// Test LPD manager default creation
#[test]
fn test_lpd_manager_default_creation() {
    let manager = LpdManager::default();

    // Should be available
    assert!(manager.is_available());

    // Should have empty active hashes
    let active = manager.active_hashes.blocking_read();
    assert!(active.is_empty());
}

/// Test LPD peer expiration check
#[test]
fn test_lpd_peer_expiration_check() {
    let peer = LpdPeer::new(test_info_hash(), 6881, test_ip());

    // Fresh peer should not be expired
    assert!(
        !peer.is_expired(Duration::from_secs(3600)),
        "Fresh peer should not be expired"
    );

    // Peer should be expired with zero tolerance
    assert!(
        peer.is_expired(Duration::ZERO),
        "Peer should be expired with zero tolerance"
    );
}

/// Test LPD peer is_local flag for private addresses
#[test]
fn test_lpd_peer_is_local_flag() {
    // Private IP addresses should have is_local = true
    let peer_private = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(192, 168, 1, 100)),
    );
    assert!(peer_private.is_local, "192.168.x.x should be local");

    let peer_loopback = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
    );
    assert!(peer_loopback.is_local, "127.x.x.x should be local");

    let peer_10 = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
    );
    assert!(peer_10.is_local, "10.x.x.x should be local");

    // Public IP address should have is_local = false
    let peer_public = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
    );
    assert!(!peer_public.is_local, "8.8.8.8 should not be local");
}

/// Test LPD real-world BEP 14 announcement parsing
#[test]
fn test_lpd_real_world_announcement_parsing() {
    // Simulate a real-world LPD announcement per BEP 14
    let info_hash = "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0";
    let port = 51413u16; // Common BT port

    let announcement = build_bep14_announcement(info_hash, port);

    let result = parse_lpd_announcement(
        announcement.as_bytes(),
        IpAddr::V4(Ipv4Addr::new(192, 168, 0, 100)),
    );
    assert!(
        result.is_some(),
        "Real-world-like announcement should parse"
    );

    let peer = result.unwrap();
    assert_eq!(peer.info_hash, info_hash);
    assert_eq!(peer.port, port);
}

// =========================================================================
// Section 4: LPD Receive Loop Integration Tests
// =========================================================================

/// Test LpdManager receive loop start/stop lifecycle
#[tokio::test]
async fn test_lpd_manager_receive_loop_lifecycle() {
    let mut manager = LpdManager::default();

    // Initially not running
    assert!(
        !manager.is_receive_loop_running(),
        "Receive loop should not be running initially"
    );

    // Start the receive loop (may fail in CI environments without multicast)
    let result = manager.start_receive_loop().await;

    if result.is_ok() {
        assert!(
            manager.is_receive_loop_running(),
            "Receive loop should be running after start"
        );

        // Stop the receive loop
        manager.stop_receive_loop().await;
        assert!(
            !manager.is_receive_loop_running(),
            "Receive loop should not be running after stop"
        );
    }
}

/// Test LpdManager receive loop cancellation token
#[tokio::test]
async fn test_lpd_manager_receive_loop_cancellation_token() {
    let mut manager = LpdManager::default();
    let result = manager.start_receive_loop().await;

    if result.is_ok() {
        let token = manager.receive_loop_cancellation_token();
        assert!(
            !token.is_cancelled(),
            "Token should not be cancelled initially"
        );

        // Cancel via token
        token.cancel();
        assert!(
            token.is_cancelled(),
            "Token should be cancelled after cancel()"
        );

        // Wait for task to notice
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !manager.is_receive_loop_running(),
            "Loop should not be running after cancellation"
        );
    }
}

/// Test that receive loop with registered torrents works
#[tokio::test]
async fn test_lpd_manager_receive_loop_with_registered_torrents() {
    let mut manager = LpdManager::default();

    // Register a torrent before starting the receive loop
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register torrent");

    let result = manager.start_receive_loop().await;

    if result.is_ok() {
        assert!(manager.is_receive_loop_running());

        // The receive loop is running and will process announcements
        // for the registered info hash
        tokio::time::sleep(Duration::from_millis(100)).await;

        manager.stop_receive_loop().await;
        assert!(!manager.is_receive_loop_running());
    }
}

/// Test receive loop start when LPD is disabled
#[tokio::test]
async fn test_lpd_manager_receive_loop_disabled() {
    // Create a manager that fails to start the receive loop
    // (in most environments, the manager is still available but
    // the multicast socket bind may fail)
    let mut manager = LpdManager::default();
    let _ = manager.start_receive_loop().await;
    // Should not panic regardless of the result
}

/// Test multiple start/stop cycles
#[tokio::test]
async fn test_lpd_manager_receive_loop_multiple_cycles() {
    let mut manager = LpdManager::default();

    for _ in 0..3 {
        let result = manager.start_receive_loop().await;
        if result.is_ok() {
            assert!(manager.is_receive_loop_running());
            manager.stop_receive_loop().await;
            assert!(!manager.is_receive_loop_running());
        } else {
            // Multicast unavailable, skip cycle
            break;
        }
    }
}

/// Test that receive loop stop is idempotent
#[tokio::test]
async fn test_lpd_manager_receive_loop_stop_idempotent() {
    let mut manager = LpdManager::default();
    let result = manager.start_receive_loop().await;

    if result.is_ok() {
        manager.stop_receive_loop().await;
        // Calling stop again should be safe
        manager.stop_receive_loop().await;
        assert!(!manager.is_receive_loop_running());
    }
}

/// Test that the receive loop doesn't interfere with peer management
#[tokio::test]
async fn test_lpd_manager_receive_loop_peer_management_independent() {
    let mut manager = LpdManager::default();
    manager
        .register_torrent(test_info_hash(), false)
        .await
        .expect("Failed to register");

    // Add peers regardless of receive loop state
    let peer = LpdPeer::new(
        test_info_hash(),
        6881,
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
    );
    manager.update_peers(test_info_hash(), vec![peer]).await;

    let _ = manager.start_receive_loop().await;

    // Peers should still be accessible
    let peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(
        peers.len(),
        1,
        "Peers should still be accessible while receive loop runs"
    );

    manager.stop_receive_loop().await;

    // Peers should still be there after stopping
    let peers = manager.get_peers_for(test_info_hash()).await;
    assert_eq!(peers.len(), 1, "Peers should survive receive loop stop");
}
