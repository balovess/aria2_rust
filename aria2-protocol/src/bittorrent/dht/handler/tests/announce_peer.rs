use super::*;

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
