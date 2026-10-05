use std::net::SocketAddr;
use std::time::{Duration, Instant};

use super::UdpError;
use aria2_protocol::bittorrent::tracker::udp_tracker_protocol::UdpAction;

use super::*;

async fn new_direct_client(bind_port: u16) -> Result<UdpTrackerClient, String> {
    UdpTrackerClient::new_with_policy(bind_port, &crate::network::OutboundNetworkPolicy::direct())
        .await
}

#[tokio::test]
async fn test_client_creation() {
    let client = new_direct_client(0).await;
    assert!(
        client.is_ok(),
        "UDP client creation should succeed with port 0"
    );
    let c = client.unwrap();
    assert!(c.pending.is_empty());
    assert!(c.inflight.is_empty());
    assert!(c.waiting_for_conn.is_empty());
}

#[tokio::test]
async fn policy_udp_client_binds_the_requested_dual_stack_family() {
    let policy = crate::network::OutboundNetworkPolicy::new(vec![
        "127.0.0.2".parse().expect("parse IPv4 source"),
        "::1".parse().expect("parse IPv6 source"),
    ])
    .expect("dual-stack policy should build");

    let ipv4 = UdpTrackerClient::new_with_policy_for_family(0, &policy, false)
        .await
        .expect("IPv4 UDP tracker client should bind");
    assert_eq!(
        ipv4.socket
            .local_addr()
            .expect("read IPv4 UDP address")
            .ip(),
        "127.0.0.2".parse::<std::net::IpAddr>().unwrap()
    );

    let ipv6 = UdpTrackerClient::new_with_policy_for_family(0, &policy, true)
        .await
        .expect("IPv6 UDP tracker client should bind");
    assert_eq!(
        ipv6.socket
            .local_addr()
            .expect("read IPv6 UDP address")
            .ip(),
        "::1".parse::<std::net::IpAddr>().unwrap()
    );
}

#[tokio::test]
async fn test_add_announce_request() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0xABu8; 20];
    let pid = [0xCDu8; 20];

    client.add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
    assert_eq!(client.pending.len(), 1);

    client.add_announce(&addr, &ih, &pid, 500, 500, 0, UdpEvent::None, -1, 6881);
    assert_eq!(client.pending.len(), 2);
}

#[tokio::test]
async fn udp_connection_id_is_reacquired_after_one_minute() {
    let tracker = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind loopback UDP tracker");
    let tracker_addr = tracker.local_addr().unwrap();
    let mut client = new_direct_client(0).await.unwrap();
    client.conn_cache.insert(
        tracker_addr,
        ConnectionState {
            id: 0x1234_5678_9abc_def0,
            updated_at: Instant::now() - Duration::from_secs(61),
        },
    );
    client.add_announce(
        &tracker_addr,
        &[0x12; 20],
        &[0x34; 20],
        0,
        1,
        0,
        UdpEvent::Started,
        50,
        6881,
    );

    assert!(client.process_one().await);
    let mut packet = [0u8; 1500];
    let (length, _) = tokio::time::timeout(Duration::from_secs(1), tracker.recv_from(&mut packet))
        .await
        .expect("client should send a UDP request")
        .expect("receive UDP request");
    assert_eq!(length, 16, "expired IDs require a CONNECT packet");
    assert_eq!(
        i32::from_be_bytes(packet[8..12].try_into().unwrap()),
        UdpAction::Connect as i32,
        "client must reacquire its expired connection ID before ANNOUNCE"
    );
}

#[tokio::test]
async fn test_process_one_needs_connection() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x12u8; 20];
    let pid = [0x34u8; 20];

    client.add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
    let processed = client.process_one().await;
    assert!(processed, "Should have processed the connect step");
    assert!(
        !client.inflight.is_empty(),
        "Should have an in-flight CONNECT"
    );
}

#[tokio::test]
async fn test_no_pending_returns_false_when_empty() {
    let mut client = new_direct_client(0).await.unwrap();
    assert!(client.pending.is_empty());
    assert!(client.inflight.is_empty());
    assert!(client.waiting_for_conn.is_empty());
    let processed = client.process_one().await;
    assert!(!processed, "process_one should return false when empty");
}

#[tokio::test]
async fn test_handle_connect_response() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x11u8; 20];
    let pid = [0x22u8; 20];

    client.add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
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
async fn connect_response_from_unexpected_udp_source_is_ignored() {
    let tracker = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind expected tracker endpoint");
    let tracker_addr = tracker.local_addr().unwrap();
    let impostor = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind unexpected response source");
    let impostor_addr = impostor.local_addr().unwrap();
    let mut client = new_direct_client(0).await.unwrap();

    client.send_connect(tracker_addr).await;
    let mut request = [0u8; 64];
    let (request_len, request_source) = tracker.recv_from(&mut request).await.unwrap();
    assert_eq!(request_len, 16);
    assert_eq!(request_source.ip(), std::net::Ipv4Addr::LOCALHOST);
    let txn_id = u32::from_be_bytes(request[12..16].try_into().unwrap());

    let mut response = Vec::with_capacity(16);
    response.extend_from_slice(&0i32.to_be_bytes());
    response.extend_from_slice(&txn_id.to_be_bytes());
    response.extend_from_slice(&0x1020_3040_5060_7080u64.to_be_bytes());
    impostor.send_to(&response, request_source).await.unwrap();
    assert!(
        client
            .receive_next_with_timeout(Duration::from_secs(1))
            .await
    );

    assert_eq!(
        client.inflight.len(),
        1,
        "foreign source must not consume the request"
    );
    assert!(client.txn_map.contains_key(&txn_id));
    assert!(!client.conn_cache.contains_key(&impostor_addr));

    tracker.send_to(&response, request_source).await.unwrap();
    assert!(
        client
            .receive_next_with_timeout(Duration::from_secs(1))
            .await
    );
    assert!(client.inflight.is_empty());
    assert_eq!(
        client.conn_cache.get(&tracker_addr).unwrap().id,
        0x1020_3040_5060_7080
    );
    assert!(!client.conn_cache.contains_key(&impostor_addr));
}

#[tokio::test]
async fn malformed_or_mismatched_connect_response_does_not_consume_transaction() {
    let tracker = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind local UDP tracker");
    let tracker_addr = tracker.local_addr().unwrap();
    let mut client = new_direct_client(0).await.unwrap();

    client.send_connect(tracker_addr).await;
    let mut request = [0u8; 64];
    let (request_len, request_source) = tracker.recv_from(&mut request).await.unwrap();
    assert_eq!(request_len, 16);
    let txn_id = u32::from_be_bytes(request[12..16].try_into().unwrap());

    let mut malformed_connect = Vec::with_capacity(17);
    malformed_connect.extend_from_slice(&0i32.to_be_bytes());
    malformed_connect.extend_from_slice(&txn_id.to_be_bytes());
    malformed_connect.extend_from_slice(&0x1020_3040_5060_7080u64.to_be_bytes());
    malformed_connect.push(0);
    tracker
        .send_to(&malformed_connect, request_source)
        .await
        .unwrap();
    assert!(
        client
            .receive_next_with_timeout(Duration::from_secs(1))
            .await
    );
    assert_eq!(
        client.inflight.len(),
        1,
        "CONNECT response must be exactly 16 bytes"
    );
    assert!(client.txn_map.contains_key(&txn_id));

    let mut mismatched_action = vec![0u8; 20];
    mismatched_action[0..4].copy_from_slice(&1i32.to_be_bytes());
    mismatched_action[4..8].copy_from_slice(&txn_id.to_be_bytes());
    tracker
        .send_to(&mismatched_action, request_source)
        .await
        .unwrap();
    assert!(
        client
            .receive_next_with_timeout(Duration::from_secs(1))
            .await
    );
    assert_eq!(
        client.inflight.len(),
        1,
        "ANNOUNCE cannot complete a CONNECT transaction"
    );
    assert!(client.txn_map.contains_key(&txn_id));

    let valid_connect = &malformed_connect[..16];
    tracker
        .send_to(valid_connect, request_source)
        .await
        .unwrap();
    assert!(
        client
            .receive_next_with_timeout(Duration::from_secs(1))
            .await
    );
    assert!(client.inflight.is_empty());
    assert!(client.conn_cache.contains_key(&tracker_addr));
}

#[tokio::test]
async fn test_handle_announce_response_with_peers() {
    let mut client = new_direct_client(0).await.unwrap();
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

    let completed: Vec<_> = client
        .pending
        .iter()
        .filter_map(|request| request.reply.as_ref())
        .collect();
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
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x55u8; 20];
    let pid = [0x66u8; 20];

    client.add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
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
    assert_eq!(
        client.pending.iter().find_map(|request| request.error),
        Some(UdpError::TrackerError)
    );
}

#[tokio::test]
async fn test_timeout_cleaning() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let ih = [0x77u8; 20];
    let pid = [0x88u8; 20];

    client.add_announce(&addr, &ih, &pid, 0, 1000, 0, UdpEvent::Started, 50, 6881);
    client.process_one().await;
    assert_eq!(client.inflight.len(), 1);

    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .handle_timeouts_with_timeout(Duration::from_secs(15))
        .await;
    assert_eq!(client.inflight.len(), 1, "Not yet timed out");

    for req in &mut client.inflight {
        req.dispatched_at = Some(Instant::now() - Duration::from_secs(16));
    }
    client
        .handle_timeouts_with_timeout(Duration::from_secs(15))
        .await;
    assert!(
        client.inflight.is_empty()
            || !client.pending.is_empty()
            || !client.waiting_for_conn.is_empty(),
        "Timed-out request should be moved"
    );

    for _ in 0..MAX_RETRIES.saturating_sub(1) {
        let mut req = client
            .pending
            .pop_front()
            .expect("timeout retry should be queued");
        req.dispatched_at = Some(Instant::now() - Duration::from_secs(16));
        client.inflight.push_back(req);
        client
            .handle_timeouts_with_timeout(Duration::from_secs(15))
            .await;
    }

    assert_eq!(
        client.pending.iter().find_map(|request| request.error),
        Some(UdpError::Timeout),
        "exhausted UDP retries should retain a timeout classification"
    );
}

#[tokio::test]
async fn udp_retry_uses_five_then_ten_second_deadlines_and_two_attempts() {
    let tracker = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind loopback UDP tracker");
    let addr = tracker.local_addr().unwrap();
    let mut client = new_direct_client(0).await.unwrap();
    client.conn_cache.insert(
        addr,
        ConnectionState {
            id: 0x1234_5678_9abc_def0,
            updated_at: Instant::now(),
        },
    );
    client.add_announce(
        &addr,
        &[0x77; 20],
        &[0x88; 20],
        0,
        1000,
        0,
        UdpEvent::Started,
        50,
        6881,
    );
    assert!(client.process_one().await);
    assert_eq!(client.inflight.len(), 1);

    client.inflight[0].dispatched_at = Some(Instant::now() - Duration::from_secs(6));
    client
        .handle_timeouts_with_timeout(Duration::from_secs(60))
        .await;
    assert!(
        client.inflight.is_empty(),
        "first attempt expires at five seconds"
    );
    assert_eq!(client.pending.front().unwrap().fail_count, 1);

    assert!(
        client.process_one().await,
        "the request should be retransmitted"
    );
    client.inflight[0].dispatched_at = Some(Instant::now() - Duration::from_secs(11));
    client
        .handle_timeouts_with_timeout(Duration::from_secs(60))
        .await;
    assert!(client.inflight.is_empty());
    assert_eq!(
        client.pending.front().and_then(|request| request.error),
        Some(UdpError::Timeout),
        "the retransmission expires after ten seconds without a third send"
    );
}

#[tokio::test(start_paused = true)]
async fn udp_receive_wait_is_cut_short_by_the_retry_deadline() {
    let tracker = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind silent loopback UDP tracker");
    let mut client = new_direct_client(0).await.unwrap();
    assert!(client.send_connect(tracker.local_addr().unwrap()).await);
    let started = tokio::time::Instant::now();

    assert!(
        !client
            .receive_next_with_timeout(Duration::from_secs(60))
            .await
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(5),
        "a 60-second tracker timeout must not postpone the first UDP retry"
    );
}

#[tokio::test]
async fn test_txn_id_generation() {
    let mut client = new_direct_client(0).await.unwrap();
    let mut txn_ids = Vec::new();
    for index in 0..5 {
        let txn_id = client.next_txn();
        txn_ids.push(txn_id);
        client.txn_map.insert(txn_id, index);
    }
    let unique: std::collections::HashSet<_> = txn_ids.iter().cloned().collect();
    assert_eq!(
        unique.len(),
        txn_ids.len(),
        "Transaction IDs should all be unique"
    );
}

// --- Scrape tests ---

#[tokio::test]
async fn test_add_scrape_request() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0xAAu8; 20], [0xBBu8; 20], [0xCCu8; 20]];

    client.add_scrape(&addr, &hashes);
    assert_eq!(client.pending.len(), 1);

    let req = &client.pending[0];
    assert!(!req.scrape_info_hashes.is_empty());
    assert_eq!(req.scrape_info_hashes.len(), 3);
    assert_eq!(req.scrape_info_hashes[0], [0xAAu8; 20]);
}

#[tokio::test]
async fn test_handle_scrape_response_single_hash() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0x11u8; 20]];

    // Manually set up an in-flight scrape request
    let txn_id = client.next_txn();
    let mut req = UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
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

    let scrape_results = client.pending[0].scrape_results.as_ref().unwrap();
    assert_eq!(scrape_results.len(), 1);
    assert_eq!(scrape_results[0].seeders, 42);
    assert_eq!(scrape_results[0].leechers, 10);
    assert_eq!(scrape_results[0].completed, 999);
}

#[tokio::test]
async fn test_handle_scrape_response_multi_hash() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0x22u8; 20], [0x33u8; 20]];

    let txn_id = client.next_txn();
    let mut req = UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
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

    let scrape_results = client.pending[0].scrape_results.as_ref().unwrap();
    assert_eq!(scrape_results.len(), 2);
    assert_eq!(scrape_results[0].seeders, 100);
    assert_eq!(scrape_results[1].seeders, 5);
}

#[tokio::test]
async fn test_scrape_error_action_returns_error() {
    let mut client = new_direct_client(0).await.unwrap();
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

    // The error must not be reported as a successful scrape.
    let scrape_results = client.pending[0].scrape_results.as_ref();
    assert!(
        scrape_results.is_none(),
        "Error response should not produce scrape results"
    );
}
