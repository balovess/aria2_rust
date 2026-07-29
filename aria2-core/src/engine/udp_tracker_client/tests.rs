use std::net::SocketAddr;
use std::time::{Duration, Instant};

use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpAction;

use super::*;

#[tokio::test]
async fn test_client_creation() {
    let client = UdpTrackerClient::new(0).await;
    assert!(
        client.is_ok(),
        "UDP client creation should succeed with port 0"
    );
    let c = client.unwrap();
    assert!(c.no_pending());
    assert!(c.completed_requests().is_empty());
}

#[tokio::test]
async fn test_add_announce_request() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0xABu8; 20];
    let pid = [0xCDu8; 20];

    client
        .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
        .await;
    assert_eq!(client.pending.len(), 1);

    client
        .add_announce(&addr, &ih, &pid, 500, 500, 0, UdpEvent::None, -1, 6881)
        .await;
    assert_eq!(client.pending.len(), 2);
}

#[tokio::test]
async fn test_process_one_needs_connection() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x12u8; 20];
    let pid = [0x34u8; 20];

    client
        .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
        .await;
    let processed = client.process_one().await;
    assert!(processed, "Should have processed the connect step");
    assert!(
        !client.inflight.is_empty(),
        "Should have an in-flight CONNECT"
    );
}

#[tokio::test]
async fn test_no_pending_returns_false_when_empty() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    assert!(client.no_pending());
    let processed = client.process_one().await;
    assert!(!processed, "process_one should return false when empty");
}

#[tokio::test]
async fn test_handle_connect_response() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x11u8; 20];
    let pid = [0x22u8; 20];

    client
        .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
        .await;
    client.process_one().await;

    let mut resp_data = vec![0u8; 16];
    resp_data[0..4].copy_from_slice(&0i32.to_be_bytes());
    let txn_id = client.inflight.front().map(|r| r.txn_id).unwrap_or(0);
    resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
    resp_data[8..16].copy_from_slice(&0x123456789ABCDEF0u64.to_be_bytes());

    client.handle_response(&resp_data, &addr).await;
    assert!(
        client.conn_cache.contains_key(&addr),
        "Should cache connection after CONNECT response"
    );
}

#[tokio::test]
async fn test_handle_announce_response_with_peers() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();

    let ih = [0x33u8; 20];
    let pid = [0x44u8; 20];
    let txn_id = client.next_txn();

    client.txn_map.insert(txn_id, 0);
    let mut dummy_req =
        UdpTrackerRequest::new(addr, ih, pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
    dummy_req.txn_id = txn_id;
    dummy_req.dispatched_at = Some(Instant::now());
    client.inflight.push_back(dummy_req);

    let mut resp_data = vec![0u8; 26];
    resp_data[0..4].copy_from_slice(&1i32.to_be_bytes());
    resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
    resp_data[8..12].copy_from_slice(&900u32.to_be_bytes());
    resp_data[12..16].copy_from_slice(&5u32.to_be_bytes());
    resp_data[16..20].copy_from_slice(&3u32.to_be_bytes());
    resp_data.extend_from_slice(&[10, 0, 0, 1, 0x1A, 0x04, 192, 168, 1, 100, 0x1F, 0x90]);

    client.handle_response(&resp_data, &addr).await;

    let completed = client.completed_requests();
    assert!(
        !completed.is_empty(),
        "Should have at least one completed announce"
    );
    assert!(
        completed[0].peers.len() >= 2,
        "Should have at least 2 peers"
    );
    assert_eq!(completed[0].interval, 900);
    assert_eq!(completed[0].leechers, 5);
    assert_eq!(completed[0].seeders, 3);
}

#[tokio::test]
async fn test_handle_error_response() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x55u8; 20];
    let pid = [0x66u8; 20];

    client
        .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
        .await;
    client.process_one().await;

    let txn_id = client.inflight.front().map(|r| r.txn_id).unwrap_or(0);
    let mut err_data = vec![0u8; 23];
    err_data[0..4].copy_from_slice(&3i32.to_be_bytes());
    err_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
    err_data[8..23].copy_from_slice(b"tracker offline");

    client.handle_response(&err_data, &addr).await;
    assert!(
        !client.conn_cache.contains_key(&addr),
        "Error should not create cache entry"
    );
}

#[tokio::test]
async fn test_timeout_cleaning() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x77u8; 20];
    let pid = [0x88u8; 20];

    client
        .add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881)
        .await;
    client.process_one().await;
    assert_eq!(client.inflight.len(), 1);

    tokio::time::sleep(Duration::from_millis(100)).await;
    client.handle_timeouts().await;
    assert_eq!(client.inflight.len(), 1, "Not yet timed out");

    for req in &mut client.inflight {
        req.dispatched_at =
            Some(Instant::now() - Duration::from_secs(REQUEST_TIMEOUT_SECS + 1));
    }
    client.handle_timeouts().await;
    assert!(
        client.inflight.is_empty()
            || !client.pending.is_empty()
            || !client.waiting_for_conn.is_empty(),
        "Timed-out request should be moved"
    );
}

#[tokio::test]
async fn test_txn_id_generation() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let mut txn_ids = Vec::new();
    for _ in 0..5 {
        txn_ids.push(client.next_txn());
        client.next_txn();
    }
    let unique: std::collections::HashSet<_> = txn_ids.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        txn_ids.len(),
        "Transaction IDs should all be unique"
    );
}

#[tokio::test]
async fn test_shared_client_creation() {
    let shared = UdpTrackerClient::create_shared(0).await;
    assert!(shared.is_ok(), "Shared client creation should succeed");
}

// --- Scrape tests ---

#[tokio::test]
async fn test_add_scrape_request() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0xAAu8; 20], [0xBBu8; 20], [0xCCu8; 20]];

    client.add_scrape(&addr, &hashes).await;
    assert_eq!(client.pending.len(), 1);

    let req = &client.pending[0];
    assert!(!req.scrape_info_hashes.is_empty());
    assert_eq!(req.scrape_info_hashes.len(), 3);
    assert_eq!(req.scrape_info_hashes[0], [0xAAu8; 20]);
}

#[tokio::test]
async fn test_handle_scrape_response_single_hash() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0x11u8; 20]];

    // Manually set up an in-flight scrape request
    let txn_id = client.next_txn();
    let mut req =
        UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
    req.txn_id = txn_id;
    req.dispatched_at = Some(Instant::now());
    req.scrape_info_hashes = hashes.to_vec();
    client.inflight.push_back(req);
    client.txn_map.insert(txn_id, 0);

    // Build scrape response: action=2, txn_id, seeders=42, leechers=10, completed=999
    let mut resp_data = vec![0u8; 20];
    resp_data[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
    resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
    resp_data[8..12].copy_from_slice(&42u32.to_be_bytes());
    resp_data[12..16].copy_from_slice(&10u32.to_be_bytes());
    resp_data[16..20].copy_from_slice(&999u32.to_be_bytes());

    client.handle_response(&resp_data, &addr).await;

    let scrape_results = client.completed_scrape_results();
    assert_eq!(scrape_results.len(), 1);
    assert_eq!(scrape_results[0].len(), 1);
    assert_eq!(scrape_results[0][0].seeders, 42);
    assert_eq!(scrape_results[0][0].leechers, 10);
    assert_eq!(scrape_results[0][0].completed, 999);
}

#[tokio::test]
async fn test_handle_scrape_response_multi_hash() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0x22u8; 20], [0x33u8; 20]];

    let txn_id = client.next_txn();
    let mut req =
        UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
    req.txn_id = txn_id;
    req.dispatched_at = Some(Instant::now());
    req.scrape_info_hashes = hashes.to_vec();
    client.inflight.push_back(req);
    client.txn_map.insert(txn_id, 0);

    // Response for 2 info hashes
    let mut resp_data = vec![0u8; 32]; // 8 header + 12*2
    resp_data[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
    resp_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
    // Hash 1
    resp_data[8..12].copy_from_slice(&100u32.to_be_bytes());
    resp_data[12..16].copy_from_slice(&50u32.to_be_bytes());
    resp_data[16..20].copy_from_slice(&200u32.to_be_bytes());
    // Hash 2
    resp_data[20..24].copy_from_slice(&5u32.to_be_bytes());
    resp_data[24..28].copy_from_slice(&3u32.to_be_bytes());
    resp_data[28..32].copy_from_slice(&7u32.to_be_bytes());

    client.handle_response(&resp_data, &addr).await;

    let scrape_results = client.completed_scrape_results();
    assert_eq!(scrape_results.len(), 1);
    assert_eq!(scrape_results[0].len(), 2);
    assert_eq!(scrape_results[0][0].seeders, 100);
    assert_eq!(scrape_results[0][1].seeders, 5);
}

#[tokio::test]
async fn test_scrape_error_action_returns_error() {
    let mut client = UdpTrackerClient::new(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();

    let txn_id = client.next_txn();
    let mut req =
        UdpTrackerRequest::new(addr, [0x99u8; 20], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
    req.txn_id = txn_id;
    req.dispatched_at = Some(Instant::now());
    req.scrape_info_hashes = vec![[0x99u8; 20]];
    client.inflight.push_back(req);
    client.txn_map.insert(txn_id, 0);

    // Send error action instead of scrape action
    let mut err_data = vec![0u8; 23];
    err_data[0..4].copy_from_slice(&3i32.to_be_bytes()); // Error action
    err_data[4..8].copy_from_slice(&txn_id.to_be_bytes());
    err_data[8..23].copy_from_slice(b"scrape failed!!");

    client.handle_response(&err_data, &addr).await;

    // Should not have successful scrape results
    let scrape_results = client.completed_scrape_results();
    assert!(
        scrape_results.is_empty(),
        "Error response should not produce scrape results"
    );
}
