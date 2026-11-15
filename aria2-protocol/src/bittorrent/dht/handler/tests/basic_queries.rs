use super::*;

#[test]
fn test_handle_ping() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    let query = DhtMessageBuilder::ping(1234, &[0xBBu8; 20]);
    let result = handler.handle_query(
        &query,
        "10.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );

    assert!(result.response.is_some());
    let resp = result.response.unwrap();
    assert!(resp.is_response());
    assert_eq!(result.sender_to_promote, Some([0xBBu8; 20]));
}

#[test]
fn test_ignores_query_from_local_node() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    let query = DhtMessageBuilder::ping(1234, &[0xAAu8; 20]);
    let result = handler.handle_query(
        &query,
        "127.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );

    assert!(result.response.is_none());
    assert!(result.sender_to_promote.is_none());
}

#[test]
fn test_handle_find_node() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    let query = DhtMessageBuilder::find_node(5678, &[0xBBu8; 20], &[0x05u8; 20]);
    let result = handler.handle_query(
        &query,
        "10.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );

    assert!(result.response.is_some());
    let resp = result.response.unwrap();
    assert!(resp.is_response());

    // Should contain nodes in the response
    let r = resp.r.as_ref().unwrap();
    let nodes = r.dict_get(b"nodes").and_then(|v| v.as_bytes());
    assert!(nodes.is_some());
    // 8 nodes × 26 bytes each for IPv4
    assert_eq!(nodes.unwrap().len(), 8 * 26);
}

#[test]
fn ipv6_find_node_response_serializes_closest_nodes_as_nodes6() {
    let handler = make_handler();
    let mut rt = RoutingTable::new([0xAA; 20]);
    for index in 1..=8u8 {
        rt.insert(DhtNode::new(
            [index; 20],
            format!("[2001:db8::{index}]:6881").parse().unwrap(),
        ));
    }
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let query = DhtMessageBuilder::find_node(1234, &[0xBB; 20], &[0x05; 20]);
    let handled = handler.handle_query(
        &query,
        "[2001:db8::100]:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    let encoded = handled.response.unwrap().encode();
    let response = DhtMessage::decode(&encoded).expect("valid find_node wire response");
    let body = response.r.expect("find_node response body");

    assert!(body.dict_get(b"nodes").is_none());
    let nodes6 = body
        .dict_get(b"nodes6")
        .and_then(BencodeValue::as_bytes)
        .expect("IPv6 find_node response must use the BEP 5 nodes6 field");
    assert_eq!(nodes6.len(), 8 * 38);
}

#[test]
fn test_handle_get_peers_no_peers() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    let query = DhtMessageBuilder::get_peers(9999, &[0xBBu8; 20], &[0xCCu8; 20]);
    let result = handler.handle_query(
        &query,
        "10.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );

    let resp = result.response.unwrap();
    let r = resp.r.as_ref().unwrap();
    // Should have nodes (no peers known)
    assert!(r.dict_get(b"nodes").is_some());
    // Should have a token
    assert!(r.dict_get(b"token").is_some());
}

#[test]
fn test_sample_infohashes_counts_peer_store_keys() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let info_hash = [0x44; 20];
    ps.add_peer(info_hash, "127.0.0.1:6881".parse().unwrap());
    let query = super::super::super::modern::sample_infohashes_query(1, &[0xBB; 20], &info_hash);
    let result = handler.handle_query(
        &query,
        "10.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    let response = result.response.unwrap();
    let body = response.r.unwrap();
    assert_eq!(
        body.dict_get(b"num").and_then(|value| value.as_int()),
        Some(1)
    );
    assert_eq!(
        body.dict_get(b"samples")
            .and_then(|value| value.as_bytes())
            .map(|value| value.len()),
        Some(20)
    );
}

#[test]
fn sample_infohashes_returns_an_exact_count_and_bounded_unique_sample() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    for value in 0..40u8 {
        ps.add_peer(
            [value; 20],
            format!("127.0.0.1:{}", 7000 + u16::from(value))
                .parse()
                .unwrap(),
        );
    }
    let query = super::super::super::modern::sample_infohashes_query(1, &[0xBB; 20], &[0x44; 20]);
    let result = handler.handle_query(
        &query,
        "10.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    let response = result.response.unwrap();
    let body = response.r.unwrap();
    let samples = body
        .dict_get(b"samples")
        .and_then(|value| value.as_bytes())
        .expect("BEP 51 samples must be present");
    let (sample_hashes, remainder) = samples.as_chunks::<20>();
    assert!(remainder.is_empty());
    let hashes = sample_hashes
        .iter()
        .copied()
        .collect::<std::collections::HashSet<_>>();

    assert_eq!(
        body.dict_get(b"num").and_then(|value| value.as_int()),
        Some(40)
    );
    assert_eq!(samples.len(), 32 * 20);
    assert_eq!(hashes.len(), 32, "sampled info hashes must be unique");
}
