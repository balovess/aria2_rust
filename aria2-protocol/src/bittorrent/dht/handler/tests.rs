use std::collections::BTreeMap;

use super::*;
use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::dht::modern::{MutableValue, StoredItem};
use crate::bittorrent::dht::routing_table::RoutingTable;

fn make_handler() -> DhtQueryHandler {
    DhtQueryHandler::new([0xAAu8; 20])
}

fn make_routing_table() -> RoutingTable {
    let mut rt = RoutingTable::new([0xAAu8; 20]);
    // Add some nodes
    for i in 0..8u8 {
        let node = DhtNode::new([i; 20], format!("10.0.0.{}:6881", i).parse().unwrap());
        rt.insert(node);
    }
    rt
}

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
    let query = super::super::modern::sample_infohashes_query(1, &[0xBB; 20], &info_hash);
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
fn test_put_rejects_non_bytes_salt() {
    use ed25519_dalek::{Signer, SigningKey};

    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let store = DhtItemStore::default();
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let mut item = MutableValue {
        public_key: key.verifying_key().to_bytes(),
        signature: [0u8; 64],
        sequence: 1,
        salt: None,
        value: BencodeValue::Bytes(b"value".to_vec()),
    };
    item.signature = key.sign(&item.signed_payload()).to_bytes();
    let target = StoredItem::mutable_target(&item.public_key, None);
    let from: SocketAddr = "10.0.0.1:6881".parse().unwrap();
    let token = tt.generate_token(&target, &from);
    let mut query = super::super::modern::put_query(7, &[0xBB; 20], token.as_bytes(), &item, None);
    if let Some(BencodeValue::Dict(args)) = query.a.as_mut() {
        args.insert(b"salt".to_vec(), BencodeValue::Int(1));
    } else {
        panic!("PUT query must have a dictionary of arguments");
    }

    let result = handler.handle_query(&query, from, &rt, &tt, &ps, Some(&store));
    assert!(result.response.is_some_and(|response| response.is_error()));
    assert!(store.get(&target).is_none());
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
    // Should NOT have nodes
    assert!(r.dict_get(b"nodes").is_none());
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

#[test]
fn test_handle_announce_peer_valid_token() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    let info_hash = [0xDDu8; 20];
    let from: SocketAddr = "10.0.0.99:6881".parse().unwrap();

    // Generate a valid token
    let token = tt.generate_token(&info_hash, &from);

    // Build announce_peer query manually with the valid token
    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xBBu8; 20]));
    args.insert(
        b"info_hash".to_vec(),
        BencodeValue::Bytes(info_hash.to_vec()),
    );
    args.insert(b"port".to_vec(), BencodeValue::Int(5000));
    args.insert(
        b"token".to_vec(),
        BencodeValue::Bytes(token.as_bytes().to_vec()),
    );

    let query = DhtMessage::new_query(1111, "announce_peer", BencodeValue::Dict(args));
    let result = handler.handle_query(&query, from, &rt, &tt, &ps, None);

    assert!(result.response.is_some());
    let resp = result.response.unwrap();
    assert!(resp.is_response());

    // Peer should be stored
    let peers = ps.get_peers(&info_hash);
    assert_eq!(peers.len(), 1);
    assert_eq!(peers[0].port(), 5000);
}

#[test]
fn test_handle_announce_peer_invalid_token() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();

    let info_hash = [0xEEu8; 20];
    let from: SocketAddr = "10.0.0.99:6881".parse().unwrap();

    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xBBu8; 20]));
    args.insert(
        b"info_hash".to_vec(),
        BencodeValue::Bytes(info_hash.to_vec()),
    );
    args.insert(b"port".to_vec(), BencodeValue::Int(5000));
    args.insert(
        b"token".to_vec(),
        BencodeValue::Bytes(b"bad_token".to_vec()),
    );

    let query = DhtMessage::new_query(1111, "announce_peer", BencodeValue::Dict(args));
    let result = handler.handle_query(&query, from, &rt, &tt, &ps, None);

    assert!(
        result.sender_to_promote.is_none(),
        "a rejected announce must not promote its sender to a good node"
    );
    let resp = result.response.unwrap();
    assert!(resp.is_error());

    // Peer should NOT be stored
    let peers = ps.get_peers(&info_hash);
    assert!(peers.is_empty());
}

#[test]
fn announce_peer_rejects_negative_port_on_krpc_boundary() {
    let handler = make_handler();
    let rt = make_routing_table();
    let tt = TokenTracker::new();
    let ps = DhtPeerStorage::new();
    let info_hash = [0xA5; 20];
    let from: SocketAddr = "10.0.0.99:6881".parse().unwrap();
    let token = tt.generate_token(&info_hash, &from);

    let mut args = BTreeMap::new();
    args.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0xBB; 20]));
    args.insert(
        b"info_hash".to_vec(),
        BencodeValue::Bytes(info_hash.to_vec()),
    );
    args.insert(b"port".to_vec(), BencodeValue::Int(-1));
    args.insert(b"token".to_vec(), BencodeValue::Bytes(token.into_bytes()));
    let request = DhtMessage::new_query(1112, "announce_peer", BencodeValue::Dict(args));
    let request = DhtMessage::decode(&request.encode()).unwrap();

    let handled = handler.handle_query(&request, from, &rt, &tt, &ps, None);
    assert!(handled.sender_to_promote.is_none());
    let response = handled
        .response
        .expect("malformed announce must return an error");
    let response = DhtMessage::decode(&response.encode()).unwrap();

    assert_eq!(response.e.map(|(code, _)| code), Some(203));
    assert!(ps.get_peers(&info_hash).is_empty());
}
