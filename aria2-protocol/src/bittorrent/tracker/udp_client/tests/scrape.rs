use super::*;

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
async fn scrape_response_with_wrong_hash_count_is_malformed() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "127.0.0.1:6969".parse().unwrap();
    let hashes = [[0x22u8; 20], [0x33u8; 20]];
    let txn_id = client.next_txn();
    let mut request =
        UdpTrackerRequest::new(addr, hashes[0], [0u8; 20], 0, 0, 0, UdpEvent::None, 0, 0);
    request.txn_id = txn_id;
    request.dispatched_at = Some(Instant::now());
    request.scrape_info_hashes = hashes.to_vec();
    client.inflight.push_back(request);
    client.txn_map.insert(txn_id, 0);

    let mut response = vec![0u8; 20];
    response[0..4].copy_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
    response[4..8].copy_from_slice(&txn_id.to_be_bytes());
    response[8..12].copy_from_slice(&100u32.to_be_bytes());
    response[12..16].copy_from_slice(&50u32.to_be_bytes());
    response[16..20].copy_from_slice(&200u32.to_be_bytes());

    client.handle_response(&response, &addr).await;

    assert_eq!(client.pending.len(), 1);
    assert!(client.pending[0].scrape_results.is_none());
    assert_eq!(client.pending[0].error, Some(UdpError::MalformedResponse));
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
