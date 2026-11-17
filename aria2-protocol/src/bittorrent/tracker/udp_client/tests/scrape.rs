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
async fn scrape_retry_keeps_hashes_after_a_send_failure() {
    let mut client = new_direct_client(0).await.unwrap();
    let addr: SocketAddr = "[::1]:6969".parse().unwrap();
    let hashes = [[0xA5u8; 20], [0x5Au8; 20]];
    client.add_scrape(&addr, &hashes);

    let mut request = client.pending.pop_front().unwrap();
    assert!(client.send_scrape(&mut request, 0x1234).await);

    let retry = client
        .pending
        .front()
        .expect("a family-mismatched send should leave the request queued for retry");
    assert_eq!(retry.scrape_info_hashes, hashes);
    assert_eq!(retry.fail_count, 1);
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

#[tokio::test]
async fn scrape_splits_large_inputs_into_bep15_sized_requests() {
    let tracker = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind loopback UDP tracker");
    let tracker_addr = tracker.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut packet = [0u8; 2048];
        let mut scrape_sizes = Vec::new();
        let mut result_index = 0u32;

        for _ in 0..3 {
            let (length, client_addr) =
                tokio::time::timeout(Duration::from_secs(2), tracker.recv_from(&mut packet))
                    .await
                    .expect("client should send the next UDP tracker request")
                    .expect("receive UDP tracker request");
            let request = &packet[..length];
            let action = i32::from_be_bytes(request[8..12].try_into().unwrap());
            let transaction_id = request[12..16].to_vec();
            let response = if action == UdpAction::Connect as i32 {
                let mut response = Vec::with_capacity(16);
                response.extend_from_slice(&(UdpAction::Connect as i32).to_be_bytes());
                response.extend_from_slice(&transaction_id);
                response.extend_from_slice(&0x1234_5678_9abc_def0u64.to_be_bytes());
                response
            } else {
                assert_eq!(action, UdpAction::Scrape as i32);
                assert!(length >= 16);
                let hash_count = (length - 16) / 20;
                assert_eq!((length - 16) % 20, 0);
                assert!(length <= 1500, "scrape datagram should remain bounded");
                scrape_sizes.push(hash_count);

                let mut response = Vec::with_capacity(8 + hash_count * 12);
                response.extend_from_slice(&(UdpAction::Scrape as i32).to_be_bytes());
                response.extend_from_slice(&transaction_id);
                for _ in 0..hash_count {
                    result_index += 1;
                    response.extend_from_slice(&result_index.to_be_bytes());
                    response.extend_from_slice(&0u32.to_be_bytes());
                    response.extend_from_slice(&0u32.to_be_bytes());
                }
                response
            };
            tracker.send_to(&response, client_addr).await.unwrap();
        }

        scrape_sizes
    });

    let hashes = vec![[0x5au8; 20]; 75];
    let mut client = new_direct_client(0).await.unwrap();
    let results = client
        .scrape(tracker_addr, &hashes, Duration::from_secs(1))
        .await
        .expect("large scrape should complete as bounded requests");

    assert_eq!(server.await.unwrap(), vec![74, 1]);
    assert_eq!(results.len(), hashes.len());
    assert_eq!(results.first().unwrap().seeders, 1);
    assert_eq!(results.last().unwrap().seeders, 75);
}
