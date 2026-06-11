use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::{
    AsyncUdpTrackerClient, UdpAction, UdpEvent, UdpTrackerClient,
};

mod fixtures;
use fixtures::mock_udp_tracker::MockUdpTracker;

#[tokio::test]
async fn test_udp_tracker_connect() {
    // Start mock tracker
    let tracker = MockUdpTracker::new().expect("Failed to create mock tracker");
    tracker.start();

    // Give server time to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create client
    let client = UdpTrackerClient::new(&tracker.url()).expect("Failed to create client");

    // Test connection
    let result = client.connect();
    assert!(result.is_ok(), "Connect failed: {:?}", result.err());

    let conn_id = result.unwrap();
    assert_ne!(conn_id, 0, "Connection ID should not be zero");

    println!("✓ UDP tracker connect test passed");
}

#[tokio::test]
async fn test_udp_tracker_announce() {
    // Start mock tracker
    let tracker = MockUdpTracker::new().expect("Failed to create mock tracker");
    tracker.start();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create client
    let client = UdpTrackerClient::new(&tracker.url()).expect("Failed to create client");

    // Prepare announce parameters
    let info_hash = [0xABu8; 20];
    let peer_id = [0xCDu8; 20];

    // Announce
    let result = client.announce(
        &info_hash,
        &peer_id,
        6881,
        0,
        0,
        1024 * 1024,
        UdpEvent::Started,
        -1,
    );

    assert!(result.is_ok(), "Announce failed: {:?}", result.err());

    let response = result.unwrap();
    assert_eq!(response.interval, 1800);
    assert_eq!(response.seeders, 10);
    assert_eq!(response.leechers, 5);
    assert_eq!(response.peers.len(), 2);

    // Verify peer data
    assert_eq!(response.peers[0].0, "192.168.1.1");
    assert_eq!(response.peers[0].1, 6881);
    assert_eq!(response.peers[1].0, "10.0.0.1");
    assert_eq!(response.peers[1].1, 6882);

    println!("✓ UDP tracker announce test passed");
}

#[tokio::test]
async fn test_udp_tracker_scrape() {
    // Start mock tracker
    let tracker = MockUdpTracker::new().expect("Failed to create mock tracker");
    tracker.start();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create client
    let client = UdpTrackerClient::new(&tracker.url()).expect("Failed to create client");

    // Scrape multiple info hashes
    let info_hashes = [[0xABu8; 20], [0xCDu8; 20], [0xEFu8; 20]];

    let result = client.scrape(&info_hashes);
    assert!(result.is_ok(), "Scrape failed: {:?}", result.err());

    let results = result.unwrap();
    assert_eq!(results.len(), 3);

    // Verify scrape data
    for (i, r) in results.iter().enumerate() {
        assert_eq!(r.seeders, 10, "Seeders mismatch for hash {}", i);
        assert_eq!(r.leechers, 5, "Leechers mismatch for hash {}", i);
        assert_eq!(r.completed, 100, "Completed mismatch for hash {}", i);
    }

    println!("✓ UDP tracker scrape test passed");
}

#[tokio::test]
async fn test_async_udp_tracker_announce() {
    // Start mock tracker
    let tracker = MockUdpTracker::new().expect("Failed to create mock tracker");
    tracker.start();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create async client
    let client = AsyncUdpTrackerClient::new(&tracker.url()).expect("Failed to create client");

    // Prepare announce parameters
    let info_hash = [0xABu8; 20];
    let peer_id = [0xCDu8; 20];

    // Async announce
    let result = client
        .announce(
            &info_hash,
            &peer_id,
            6881,
            0,
            0,
            1024 * 1024,
            UdpEvent::Started,
            -1,
        )
        .await;

    assert!(result.is_ok(), "Async announce failed: {:?}", result.err());

    let response = result.unwrap();
    assert_eq!(response.peers.len(), 2);

    println!("✓ Async UDP tracker announce test passed");
}

#[tokio::test]
async fn test_udp_tracker_connection_caching() {
    // Start mock tracker
    let tracker = MockUdpTracker::new().expect("Failed to create mock tracker");
    tracker.start();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create client
    let client = UdpTrackerClient::new(&tracker.url()).expect("Failed to create client");

    // First announce - will connect
    let info_hash = [0xABu8; 20];
    let peer_id = [0xCDu8; 20];

    let result1 = client.announce(
        &info_hash,
        &peer_id,
        6881,
        0,
        0,
        1024 * 1024,
        UdpEvent::Started,
        -1,
    );
    assert!(result1.is_ok());

    // Second announce - should use cached connection
    let result2 = client.announce(
        &info_hash,
        &peer_id,
        6881,
        1024,
        0,
        512 * 1024,
        UdpEvent::None,
        -1,
    );
    assert!(result2.is_ok());

    println!("✓ UDP tracker connection caching test passed");
}

#[tokio::test]
async fn test_udp_tracker_multiple_events() {
    // Start mock tracker
    let tracker = MockUdpTracker::new().expect("Failed to create mock tracker");
    tracker.start();

    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    // Create client
    let client = UdpTrackerClient::new(&tracker.url()).expect("Failed to create client");

    let info_hash = [0xABu8; 20];
    let peer_id = [0xCDu8; 20];

    // Test different events
    for event in [UdpEvent::Started, UdpEvent::None, UdpEvent::Completed, UdpEvent::Stopped] {
        let result = client.announce(&info_hash, &peer_id, 6881, 0, 0, 1024 * 1024, event, -1);
        assert!(
            result.is_ok(),
            "Announce with event {:?} failed: {:?}",
            event,
            result.err()
        );
    }

    println!("✓ UDP tracker multiple events test passed");
}

#[test]
fn test_udp_action_values() {
    assert_eq!(UdpAction::Connect as i32, 0);
    assert_eq!(UdpAction::Announce as i32, 1);
    assert_eq!(UdpAction::Scrape as i32, 2);
    assert_eq!(UdpAction::Error as i32, 3);

    println!("✓ UDP action values test passed");
}

#[test]
fn test_udp_event_values() {
    assert_eq!(UdpEvent::None as i32, 0);
    assert_eq!(UdpEvent::Completed as i32, 1);
    assert_eq!(UdpEvent::Started as i32, 2);
    assert_eq!(UdpEvent::Stopped as i32, 3);

    println!("✓ UDP event values test passed");
}

#[test]
fn test_udp_tracker_client_url_parsing() {
    // Valid URLs (with IP address, not hostname)
    let client1 = UdpTrackerClient::new("udp://127.0.0.1:6969");
    assert!(client1.is_ok(), "Failed to parse valid UDP URL");

    let client2 = UdpTrackerClient::new("udp://127.0.0.1:1337");
    assert!(client2.is_ok(), "Failed to parse valid UDP URL");

    // Invalid URLs
    let client3 = UdpTrackerClient::new("http://tracker.example.com:6969");
    assert!(client3.is_err(), "Should reject HTTP URL");

    let client4 = UdpTrackerClient::new("invalid-url");
    assert!(client4.is_err(), "Should reject invalid URL");

    println!("✓ UDP tracker URL parsing test passed");
}
