use super::*;

#[test]
fn test_ipv6_get_uses_nodes6_field() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let store = DhtItemStore::default();
    let target = [0x42; 20];
    let query = super::super::super::modern::get_query(10, &[0xBE; 20], &target, None);
    let from: SocketAddr = "[2001:db8::1]:6881".parse().unwrap();

    let response = handler
        .handle_query(&query, from, &rt, &tt, &ps, Some(&store))
        .response
        .unwrap();
    let body = response.r.as_ref().unwrap();
    assert!(body.dict_get(b"nodes6").is_some());
    assert!(
        body.dict_get(b"nodes")
            .is_some_and(|value| value.as_bytes().is_some_and(|nodes| nodes.is_empty()))
    );
}

#[test]
fn test_invalid_sender_id_is_rejected_before_dispatch() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let mut query = DhtMessageBuilder::ping(7, &[0x11; 20]);
    query.a = Some(BencodeValue::Dict(BTreeMap::new()));

    let result = handler.handle_query(
        &query,
        "127.0.0.1:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    assert!(result.sender_to_promote.is_none());
    assert!(result.response.is_some_and(|response| response.is_error()));
}

#[test]
fn test_handle_get_peers_with_peers() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    // Pre-populate peer storage
    let info_hash = [0xCCu8; 20];
    ps.add_peer(info_hash, "192.168.1.1:5000".parse().unwrap());

    let query = DhtMessageBuilder::get_peers(9999, &[0xBBu8; 20], &info_hash);
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
    // Should have values (peers known)
    assert!(r.dict_get(b"values").is_some());
    // Known peers do not suppress the closest IPv4 routing nodes.
    let nodes = r
        .dict_get(b"nodes")
        .and_then(BencodeValue::as_bytes)
        .expect("closest nodes should accompany known peers");
    assert!(!nodes.is_empty() && nodes.len().is_multiple_of(26));
}

#[test]
fn get_peers_response_filters_family_and_caps_values_for_path_mtu() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let info_hash = [0xCC; 20];

    for index in 1..=30u16 {
        let ipv6: SocketAddr = format!("[2001:db8::{index}]:{}", 5000 + index)
            .parse()
            .unwrap();
        ps.add_peer(info_hash, ipv6);
    }
    for index in 1..=20u16 {
        let ipv4 = SocketAddr::new(format!("192.0.2.{index}").parse().unwrap(), 6000 + index);
        ps.add_peer(info_hash, ipv4);
    }

    let ipv6_query = DhtMessageBuilder::get_peers(9999, &[0xBB; 20], &info_hash);
    let ipv6_result = handler.handle_query(
        &ipv6_query,
        "[2001:db8::100]:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    let ipv6_response = ipv6_result.response.unwrap();
    let ipv6_values = ipv6_response
        .r
        .as_ref()
        .and_then(|result| result.dict_get(b"values"))
        .and_then(BencodeValue::as_list)
        .expect("IPv6 response should contain compact peer values");
    assert_eq!(ipv6_values.len(), 25);
    assert!(
        ipv6_values
            .iter()
            .all(|value| { value.as_bytes().is_some_and(|compact| compact.len() == 18) })
    );
    assert!(ipv6_response.encode().len() <= 1024);

    let ipv4_query = DhtMessageBuilder::get_peers(10000, &[0xBC; 20], &info_hash);
    let ipv4_result = handler.handle_query(
        &ipv4_query,
        "192.0.2.100:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    let ipv4_response = ipv4_result.response.unwrap();
    let ipv4_values = ipv4_response
        .r
        .as_ref()
        .and_then(|result| result.dict_get(b"values"))
        .and_then(BencodeValue::as_list)
        .expect("IPv4 response should contain compact peer values");
    assert_eq!(ipv4_values.len(), 20);
    assert!(
        ipv4_values
            .iter()
            .all(|value| { value.as_bytes().is_some_and(|compact| compact.len() == 6) })
    );
}

#[test]
fn get_peers_advertises_local_peer_without_duplicates_and_preserves_response_cap() {
    let info_hash = [0xCC; 20];
    let local_peer: std::net::SocketAddr = "203.0.113.37:6881".parse().unwrap();
    let lookup = Arc::new(move |hash: &[u8; 20], requester_ip: std::net::IpAddr| {
        (hash == &info_hash && requester_ip.is_ipv4()).then_some(local_peer)
    });
    let handler = make_handler().with_local_peer_lookup(lookup);
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    ps.add_peer(info_hash, local_peer);
    let duplicate_query = DhtMessageBuilder::get_peers(9999, &[0xBB; 20], &info_hash);
    let duplicate_reply = handler
        .handle_query(
            &duplicate_query,
            "192.0.2.100:6881".parse().unwrap(),
            &rt,
            &tt,
            &ps,
            None,
        )
        .response
        .unwrap();
    let duplicate_values = duplicate_reply
        .r
        .as_ref()
        .and_then(|result| result.dict_get(b"values"))
        .and_then(BencodeValue::as_list)
        .unwrap();
    assert_eq!(duplicate_values.len(), 1);

    for index in 1..=25u16 {
        ps.add_peer(
            info_hash,
            std::net::SocketAddr::new(format!("192.0.2.{index}").parse().unwrap(), 6000 + index),
        );
    }
    let capped_query = DhtMessageBuilder::get_peers(10000, &[0xBC; 20], &info_hash);
    let capped_reply = handler
        .handle_query(
            &capped_query,
            "192.0.2.100:6881".parse().unwrap(),
            &rt,
            &tt,
            &ps,
            None,
        )
        .response
        .unwrap();
    let capped_values = capped_reply
        .r
        .as_ref()
        .and_then(|result| result.dict_get(b"values"))
        .and_then(BencodeValue::as_list)
        .unwrap();
    assert_eq!(capped_values.len(), 25);
    assert!(capped_values.iter().any(|value| {
        value
            .as_bytes()
            .is_some_and(|bytes| bytes == [203, 0, 113, 37, 0x1A, 0xE1])
    }));
}

#[test]
fn ipv6_get_peers_fallback_serializes_closest_nodes_as_nodes6() {
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
    let info_hash = [0xCC; 20];
    ps.add_peer(info_hash, "192.0.2.1:5000".parse().unwrap());

    let query = DhtMessageBuilder::get_peers(1234, &[0xBB; 20], &info_hash);
    let handled = handler.handle_query(
        &query,
        "[2001:db8::100]:6881".parse().unwrap(),
        &rt,
        &tt,
        &ps,
        None,
    );
    let encoded = handled.response.unwrap().encode();
    let response = DhtMessage::decode(&encoded).expect("valid get_peers wire response");
    let body = response.r.expect("get_peers response body");

    assert!(body.dict_get(b"values").is_none());
    assert!(body.dict_get(b"nodes").is_none());
    let nodes6 = body
        .dict_get(b"nodes6")
        .and_then(BencodeValue::as_bytes)
        .expect("IPv6 fallback must use the BEP 5 nodes6 field");
    assert_eq!(nodes6.len(), 8 * 38);
}
