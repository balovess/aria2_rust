use super::*;

#[test]
fn test_ping_encode_decode_roundtrip() {
    let id = [1u8; 20];
    let msg = DhtMessageBuilder::ping(1234, &id);
    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_query());
    assert_eq!(&decoded.t, &msg.t);
}

#[test]
fn test_find_node_message() {
    let sender = [1u8; 20];
    let target = [2u8; 20];
    let msg = DhtMessageBuilder::find_node(5678, &sender, &target);
    assert!(msg.is_query());

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();
    assert_eq!(decoded.q.as_ref().unwrap().0, "find_node");
}

#[test]
fn test_error_message() {
    let msg = DhtMessage::new_error(vec![0xAA, 0xBB], 203, "Server Error");
    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();
    assert!(decoded.is_error());
    assert_eq!(decoded.e, Some((203, "Server Error".to_string())));
}

#[test]
fn test_response_message() {
    let mut result = std::collections::BTreeMap::new();
    result.insert(b"id".to_vec(), BencodeValue::Bytes(vec![0u8; 20]));
    let msg = DhtMessage::new_response(vec![0x01], BencodeValue::Dict(result));
    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();
    assert!(decoded.is_response());
}

#[test]
fn test_decode_rejects_noncanonical_message_type_values() {
    use std::collections::BTreeMap;

    for invalid_type in [b"".as_slice(), b"qjunk", b"r\0", b"x"] {
        let mut fields = BTreeMap::new();
        fields.insert(
            b"a".to_vec(),
            BencodeValue::Dict(BTreeMap::from([(
                b"id".to_vec(),
                BencodeValue::Bytes(vec![0x11; 20]),
            )])),
        );
        fields.insert(b"q".to_vec(), BencodeValue::Bytes(b"ping".to_vec()));
        fields.insert(b"t".to_vec(), BencodeValue::Bytes(vec![0, 0, 0, 1]));
        fields.insert(b"y".to_vec(), BencodeValue::Bytes(invalid_type.to_vec()));
        let encoded = BencodeValue::Dict(fields).encode();

        assert!(
            DhtMessage::decode(&encoded).is_err(),
            "message type {:?} should be rejected",
            invalid_type
        );
    }
}

#[test]
fn test_decode_rejects_response_without_valid_result_node_id() {
    use std::collections::BTreeMap;

    let invalid_results = [
        BencodeValue::Bytes(b"not a dictionary".to_vec()),
        BencodeValue::Dict(BTreeMap::new()),
        BencodeValue::Dict(BTreeMap::from([(
            b"id".to_vec(),
            BencodeValue::Bytes(vec![0x11; 19]),
        )])),
    ];

    for result in invalid_results {
        let encoded = BencodeValue::Dict(BTreeMap::from([
            (b"r".to_vec(), result),
            (b"t".to_vec(), BencodeValue::Bytes(vec![0, 0, 0, 1])),
            (b"y".to_vec(), BencodeValue::Bytes(b"r".to_vec())),
        ]))
        .encode();

        assert!(DhtMessage::decode(&encoded).is_err());
    }
}

#[test]
fn test_decode_rejects_malformed_error_body() {
    use std::collections::BTreeMap;

    let invalid_errors = [
        BencodeValue::List(vec![BencodeValue::Int(203)]),
        BencodeValue::List(vec![
            BencodeValue::Bytes(b"203".to_vec()),
            BencodeValue::Bytes(b"Protocol Error".to_vec()),
        ]),
        BencodeValue::List(vec![
            BencodeValue::Int(203),
            BencodeValue::Bytes(b"Protocol Error".to_vec()),
            BencodeValue::Int(0),
        ]),
    ];

    for error in invalid_errors {
        let encoded = BencodeValue::Dict(BTreeMap::from([
            (b"e".to_vec(), error),
            (b"t".to_vec(), BencodeValue::Bytes(vec![0, 0, 0, 1])),
            (b"y".to_vec(), BencodeValue::Bytes(b"e".to_vec())),
        ]))
        .encode();

        assert!(DhtMessage::decode(&encoded).is_err());
    }
}

// ==================== encode_compact_peer tests ====================

#[test]
fn test_encode_compact_peer_ipv4() {
    let addr: std::net::SocketAddr = "192.168.1.100:8080".parse().unwrap();
    let bytes = encode_compact_peer(addr);
    assert_eq!(bytes.len(), 6);
    assert_eq!(&bytes[0..4], &[192, 168, 1, 100]);
    assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 8080);
}

#[test]
fn test_encode_compact_peer_ipv6() {
    let addr: std::net::SocketAddr = "[2001:db8::1]:443".parse().unwrap();
    let bytes = encode_compact_peer(addr);
    assert_eq!(bytes.len(), 18);
    // First 16 bytes are the IPv6 address octets
    let expected_octets: [u8; 16] =
        std::net::Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 1).octets();
    assert_eq!(&bytes[0..16], &expected_octets[..]);
    assert_eq!(u16::from_be_bytes([bytes[16], bytes[17]]), 443);
}

// ==================== Response builder tests ====================

#[test]
fn test_ping_response_encode_decode() {
    let tx = [0x01u8, 0x02, 0x03, 0x04];
    let self_id = [0xAAu8; 20];
    let msg = DhtMessageBuilder::ping_response(&tx, &self_id);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_response());
    assert_eq!(decoded.t, tx.to_vec());

    let r = decoded.r.as_ref().expect("response must have r field");
    let id_bytes = r
        .dict_get(b"id")
        .and_then(|v| v.as_bytes())
        .expect("missing r.id");
    assert_eq!(id_bytes, &self_id[..]);
}

#[test]
fn test_find_node_response_encode_decode() {
    let tx = [0x11u8, 0x22];
    let self_id = [0xBBu8; 20];

    // Build 2 compact nodes (26 bytes each: 20 ID + 4 IP + 2 port)
    let mut compact_nodes = Vec::new();
    // Node 1: id=0x01.., IP 192.168.1.1:8080
    compact_nodes.extend_from_slice(&[0x01u8; 20]);
    compact_nodes.extend_from_slice(&[192, 168, 1, 1]);
    compact_nodes.extend_from_slice(&[0x1F, 0x90]); // 8080
    // Node 2: id=0x02.., IP 10.0.0.2:6881
    compact_nodes.extend_from_slice(&[0x02u8; 20]);
    compact_nodes.extend_from_slice(&[10, 0, 0, 2]);
    compact_nodes.extend_from_slice(&[0x1A, 0xE1]); // 6881

    let msg = DhtMessageBuilder::find_node_response(&tx, &self_id, &compact_nodes);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_response());
    assert_eq!(decoded.t, tx.to_vec());

    let r = decoded.r.as_ref().expect("response must have r field");
    let id_bytes = r
        .dict_get(b"id")
        .and_then(|v| v.as_bytes())
        .expect("missing r.id");
    assert_eq!(id_bytes, &self_id[..]);

    let nodes_bytes = r
        .dict_get(b"nodes")
        .and_then(|v| v.as_bytes())
        .expect("missing r.nodes");
    assert_eq!(nodes_bytes, &compact_nodes[..]);

    // Cross-check with the standard compact extractor.
    let extracted = crate::bittorrent::dht::compact::extract_compact_nodes_from_response(&decoded);
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted[0].0.port(), 8080);
    assert_eq!(extracted[1].0.port(), 6881);
    assert_eq!(extracted[0].1, [0x01u8; 20]);
    assert_eq!(extracted[1].1, [0x02u8; 20]);
}

#[test]
fn test_get_peers_response_with_peers_encode_decode() {
    let tx = [0xDEu8, 0xAD, 0xBE, 0xEF];
    let self_id = [0xCCu8; 20];
    let token = b"tok123";

    let peers: Vec<std::net::SocketAddr> = vec![
        "192.168.1.1:8080".parse().unwrap(),
        "10.0.0.2:6881".parse().unwrap(),
    ];

    let msg = DhtMessageBuilder::get_peers_response_with_peers(&tx, &self_id, token, &peers);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_response());
    assert_eq!(decoded.t, tx.to_vec());

    let r = decoded.r.as_ref().expect("response must have r field");
    let id_bytes = r
        .dict_get(b"id")
        .and_then(|v| v.as_bytes())
        .expect("missing r.id");
    assert_eq!(id_bytes, &self_id[..]);

    let token_bytes = r
        .dict_get(b"token")
        .and_then(|v| v.as_bytes())
        .expect("missing r.token");
    assert_eq!(token_bytes, &token[..]);

    let values = r
        .dict_get(b"values")
        .and_then(|v| v.as_list())
        .expect("missing r.values");
    assert_eq!(values.len(), 2);

    // Cross-check with the standard compact extractor.
    let extracted = crate::bittorrent::dht::compact::extract_compact_peers_from_response(&decoded);
    assert_eq!(extracted.len(), 2);
    assert_eq!(extracted[0], peers[0]);
    assert_eq!(extracted[1], peers[1]);
}

#[test]
fn test_get_peers_response_with_peers_empty() {
    let tx = [0x01u8];
    let self_id = [0x00u8; 20];
    let token = b"";
    let peers: Vec<std::net::SocketAddr> = vec![];

    let msg = DhtMessageBuilder::get_peers_response_with_peers(&tx, &self_id, token, &peers);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    let r = decoded.r.as_ref().expect("response must have r field");
    let values = r
        .dict_get(b"values")
        .and_then(|v| v.as_list())
        .expect("missing r.values");
    assert!(values.is_empty());

    let extracted = crate::bittorrent::dht::compact::extract_compact_peers_from_response(&decoded);
    assert!(extracted.is_empty());
}

#[test]
fn test_get_peers_response_with_nodes_encode_decode() {
    let tx = [0x55u8, 0x66];
    let self_id = [0xDDu8; 20];
    let token = b"node-token";

    // Build 1 compact node (26 bytes)
    let mut compact_nodes = Vec::new();
    compact_nodes.extend_from_slice(&[0x99u8; 20]);
    compact_nodes.extend_from_slice(&[172, 16, 0, 1]);
    compact_nodes.extend_from_slice(&[0x1F, 0x90]); // 8080

    let msg =
        DhtMessageBuilder::get_peers_response_with_nodes(&tx, &self_id, token, &compact_nodes);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_response());
    assert_eq!(decoded.t, tx.to_vec());

    let r = decoded.r.as_ref().expect("response must have r field");
    let id_bytes = r
        .dict_get(b"id")
        .and_then(|v| v.as_bytes())
        .expect("missing r.id");
    assert_eq!(id_bytes, &self_id[..]);

    let token_bytes = r
        .dict_get(b"token")
        .and_then(|v| v.as_bytes())
        .expect("missing r.token");
    assert_eq!(token_bytes, &token[..]);

    let nodes_bytes = r
        .dict_get(b"nodes")
        .and_then(|v| v.as_bytes())
        .expect("missing r.nodes");
    assert_eq!(nodes_bytes, &compact_nodes[..]);

    // Cross-check with the standard compact extractor.
    let extracted = crate::bittorrent::dht::compact::extract_compact_nodes_from_response(&decoded);
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].0.port(), 8080);
    assert_eq!(extracted[0].1, [0x99u8; 20]);
}

#[test]
fn test_announce_peer_response_encode_decode() {
    let tx = [0x77u8, 0x88, 0x99];
    let self_id = [0xEEu8; 20];
    let msg = DhtMessageBuilder::announce_peer_response(&tx, &self_id);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_response());
    assert_eq!(decoded.t, tx.to_vec());

    let r = decoded.r.as_ref().expect("response must have r field");
    let id_bytes = r
        .dict_get(b"id")
        .and_then(|v| v.as_bytes())
        .expect("missing r.id");
    assert_eq!(id_bytes, &self_id[..]);

    // announce_peer response should only contain "id" — no nodes/values/token
    assert!(r.dict_get(b"nodes").is_none());
    assert!(r.dict_get(b"values").is_none());
    assert!(r.dict_get(b"token").is_none());
}

#[test]
fn test_announce_peer_preserves_opaque_token_bytes() {
    let token = [0x00, 0xFF, 0x10, 0x80];
    let msg =
        DhtMessageBuilder::announce_peer_with_token(7, &[0x11; 20], &[0x22; 20], 6881, &token);

    let decoded = DhtMessage::decode(&msg.encode()).unwrap();
    let args = decoded.a.as_ref().expect("announce_peer must have args");
    let encoded_token = args
        .dict_get(b"token")
        .and_then(|value| value.as_bytes())
        .expect("announce_peer must include token");
    assert_eq!(encoded_token, &token);
}

#[test]
fn test_error_response_encode_decode() {
    let tx = [0xABu8, 0xCD];
    let msg = DhtMessageBuilder::error_response(&tx, 203, "Invalid token");

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_error());
    assert_eq!(decoded.t, tx.to_vec());
    assert_eq!(decoded.e, Some((203, "Invalid token".to_string())));
}

#[test]
fn test_error_response_generic_error() {
    let tx = [0x00u8];
    let msg = DhtMessageBuilder::error_response(&tx, 202, "Server Error");

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert!(decoded.is_error());
    assert_eq!(decoded.t, tx.to_vec());
    assert_eq!(decoded.e, Some((202, "Server Error".to_string())));
}

#[test]
fn test_ping_response_echoes_arbitrary_tx_length() {
    // BEP 0005 allows variable-length transaction IDs (typically 2 bytes).
    // Verify a 2-byte tx is echoed back correctly.
    let tx = [0x0Au8, 0x0B];
    let self_id = [0x12u8; 20];
    let msg = DhtMessageBuilder::ping_response(&tx, &self_id);

    let encoded = msg.encode();
    let decoded = DhtMessage::decode(&encoded).unwrap();

    assert_eq!(decoded.t, tx.to_vec());
}
