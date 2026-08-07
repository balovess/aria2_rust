//! PEX tests — split from pex.rs to keep file size under 600 lines.
//!
//! This module is loaded by `mod tests;` in pex.rs.

use super::*;
use std::collections::BTreeMap;
use tracing::info;

#[test]
fn test_pex_support_detection() {
    assert!(PexHandler::is_supported_by_peer(&[Some(1)]));
    assert!(!PexHandler::is_supported_by_peer(&[Some(2)]));
    assert!(!PexHandler::is_supported_by_peer(&[None]));
}

#[test]
fn test_build_pex_message() {
    let addr = PeerAddr::new("1.2.3.4", 5678);
    let msg = PexHandler::build_pex_message(std::slice::from_ref(&addr), &[]);
    assert!(msg.is_dict());
    assert!(msg.dict_get("added").is_some());
    assert!(msg.dict_get("added.f").is_some());
}

#[test]
fn test_parse_pex_ipv4_peers() {
    let peers = vec![
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("10.0.0.1", 6882),
        PeerAddr::new("172.16.0.1", 6883),
    ];

    let bencode_msg = PexHandler::build_pex_message(&peers, &[]);
    let encoded = bencode_msg.encode();

    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added {
            peers: parsed_peers,
            ..
        } => {
            assert_eq!(parsed_peers.len(), 3);
            assert_eq!(parsed_peers[0].addr.ip, "192.168.1.1");
            assert_eq!(parsed_peers[0].addr.port, 6881);
            assert_eq!(parsed_peers[1].addr.ip, "10.0.0.1");
            assert_eq!(parsed_peers[1].addr.port, 6882);
            assert_eq!(parsed_peers[2].addr.ip, "172.16.0.1");
            assert_eq!(parsed_peers[2].addr.port, 6883);
        }
        _ => panic!("Expected Added message"),
    }
}

#[test]
fn test_parse_pex_ipv6_peers() {
    let peers = vec![PeerAddr::new("::1", 6881)];

    let bencode_msg = PexHandler::build_pex_message(&peers, &[]);
    let encoded = bencode_msg.encode();
    assert!(!encoded.is_empty());
}

#[test]
fn test_build_pex_message_roundtrip() {
    let original_peers = vec![
        PeerAddr::new("192.168.1.100", 6881),
        PeerAddr::new("10.0.0.50", 6882),
        PeerAddr::new("172.16.5.1", 6883),
    ];

    let bencode_msg = PexHandler::build_pex_message(&original_peers, &[]);
    let encoded = bencode_msg.encode();

    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added { peers, .. } => {
            assert_eq!(peers.len(), original_peers.len());
            for (i, original) in original_peers.iter().enumerate() {
                assert_eq!(peers[i].addr.ip, original.ip);
                assert_eq!(peers[i].addr.port, original.port);
            }
        }
        _ => panic!("Expected Added message"),
    }
}

#[test]
fn test_encode_decode_compact_peers_v4() {
    let peers = vec![
        PeerAddr::new("1.2.3.4", 5678),
        PeerAddr::new("255.255.255.255", 65535),
        PeerAddr::new("0.0.0.0", 0),
    ];

    let encoded = encode_compact_peers_v4(&peers);
    assert_eq!(encoded.len(), peers.len() * 6);

    let decoded = decode_compact_peers_v4(&encoded).unwrap();
    assert_eq!(decoded.len(), peers.len());

    for i in 0..peers.len() {
        assert_eq!(decoded[i].ip, peers[i].ip);
        assert_eq!(decoded[i].port, peers[i].port);
    }
}

#[test]
fn test_encode_decode_compact_peers_v6() {
    let peers = vec![PeerAddr::new("2001:db8::1", 6881)];

    let encoded = encode_compact_peers_v6(&peers);
    assert_eq!(encoded.len(), 18);
}

#[test]
fn test_pex_dedup() {
    let duplicate_peers = vec![
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("10.0.0.1", 6882),
        PeerAddr::new("192.168.1.1", 6881),
    ];

    let bencode_msg = PexHandler::build_pex_message(&duplicate_peers, &[]);
    let encoded = bencode_msg.encode();

    let local_addr = PeerAddr::new("127.0.0.1", 6880);
    let (added, _) = PexHandler::process_received_pex(&encoded, &local_addr).unwrap();

    assert_eq!(added.len(), 2);
}

#[test]
fn test_process_received_filters_local_addr() {
    let peers = vec![
        PeerAddr::new("127.0.0.1", 6880),
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("10.0.0.1", 6882),
    ];

    let bencode_msg = PexHandler::build_pex_message(&peers, &[]);
    let encoded = bencode_msg.encode();

    let local_addr = PeerAddr::new("127.0.0.1", 6880);
    let (added, _) = PexHandler::process_received_pex(&encoded, &local_addr).unwrap();

    assert!(!added.iter().any(|p| p.ip == "127.0.0.1" && p.port == 6880));
    assert_eq!(added.len(), 2);
}

#[test]
fn test_build_pex_added_limits_peers() {
    let mut known_peers = Vec::new();
    for i in 0..100 {
        known_peers.push(PeerAddr::new(
            format!("192.168.1.{}", i + 1).as_str(),
            6881 + i as u16,
        ));
    }

    let remote_addr = PeerAddr::new("10.0.0.1", 6881);
    let msg = PexHandler::build_pex_added(&known_peers, &remote_addr, 50);

    let added_data = msg.dict_get("added").unwrap().as_bytes().unwrap();
    let peer_count = added_data.len() / 6;

    assert_eq!(peer_count, 50);
}

#[test]
fn test_build_pex_added_excludes_remote() {
    let known_peers = vec![
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("10.0.0.1", 6882),
        PeerAddr::new("172.16.0.1", 6883),
    ];

    let remote_addr = PeerAddr::new("10.0.0.1", 6882);
    let msg = PexHandler::build_pex_added(&known_peers, &remote_addr, 50);

    let added_data = msg.dict_get("added").unwrap().as_bytes().unwrap();
    let decoded = decode_compact_peers_v4(added_data).unwrap();

    assert!(!decoded.contains(&remote_addr));
    assert_eq!(decoded.len(), 2);
}

#[test]
fn test_parse_pex_with_flags() {
    let mut dict = BTreeMap::new();
    let peer1 = PeerAddr::new("1.2.3.4", 6881);
    let peer2 = PeerAddr::new("5.6.7.8", 6882);

    dict.insert(
        b"added".to_vec(),
        BencodeValue::Bytes(encode_compact_peers_v4(&[peer1.clone(), peer2.clone()])),
    );
    dict.insert(b"added.f".to_vec(), BencodeValue::Bytes(vec![0x03, 0x01]));

    let value = BencodeValue::Dict(dict);
    let encoded = value.encode();

    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added { peers, .. } => {
            assert_eq!(peers.len(), 2);
            assert_eq!(peers[0].flags, 0x03);
            assert_eq!(peers[1].flags, 0x01);
            assert_eq!(peers[0].addr.ip, "1.2.3.4");
            assert_eq!(peers[1].addr.ip, "5.6.7.8");
        }
        _ => panic!("Expected Added message"),
    }
}

#[test]
fn test_empty_pex_message() {
    let msg = PexHandler::build_pex_message(&[], &[]);
    let encoded = msg.encode();

    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added { peers, .. } => {
            assert!(peers.is_empty());
        }
        _ => panic!("Expected Added message"),
    }
}

// ==================== Comprehensive PEX Tests ====================

#[test]
fn test_pex_encode_decode_roundtrip_3_ipv4_peers() {
    let original_peers = vec![
        PeerAddr::new("192.168.1.10", 6881),
        PeerAddr::new("10.0.0.5", 6882),
        PeerAddr::new("172.16.0.100", 6883),
    ];

    let bencode_msg = PexHandler::build_pex_message(&original_peers, &[]);
    let encoded = bencode_msg.encode();
    assert!(!encoded.is_empty(), "Encoded PEX should not be empty");

    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added { peers, .. } => {
            assert_eq!(
                peers.len(),
                3,
                "Should have exactly 3 peers after roundtrip"
            );
            for (i, peer) in peers.iter().enumerate() {
                assert_eq!(peer.addr.ip, original_peers[i].ip, "Peer {} IP mismatch", i);
                assert_eq!(
                    peer.addr.port, original_peers[i].port,
                    "Peer {} port mismatch",
                    i
                );
            }
        }
        _ => panic!("Expected Added message with 3 peers"),
    }
}

#[test]
fn test_pex_ipv4_ipv6_mixed_format() {
    let ipv4_peers = vec![
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("10.0.0.1", 6883),
    ];

    let ipv6_peers = vec![
        PeerAddr::new("2001:db8:85a3::8a2e:370:7334", 6882),
        PeerAddr::new("fe80::1", 6884),
    ];

    let bencode_msg_v4 = PexHandler::build_pex_message(&ipv4_peers, &[]);
    let encoded_v4 = bencode_msg_v4.encode();

    let parsed_v4 = PexHandler::parse_pex_data(&encoded_v4).unwrap();
    match parsed_v4 {
        PexMessage::Added { peers, .. } => {
            assert_eq!(peers.len(), 2, "Should have 2 IPv4 peers");
            let has_ipv4 = peers.iter().all(|p| p.addr.ip.contains('.'));
            assert!(has_ipv4, "All should be IPv4");
        }
        _ => panic!("Expected Added message"),
    }

    let bencode_msg_v6 = PexHandler::build_pex_message(&ipv6_peers, &[]);
    let encoded_v6 = bencode_msg_v6.encode();
    assert!(
        encoded_v6.len() >= 2 * 18,
        "IPv6 encoded data should contain at least 2 peers worth of compact data (got {} bytes)",
        encoded_v6.len()
    );
}

#[test]
fn test_pex_dedup_logic_removes_duplicates() {
    let duplicate_peers = vec![
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("10.0.0.1", 6882),
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("172.16.0.1", 6883),
        PeerAddr::new("10.0.0.1", 6882),
    ];

    let bencode_msg = PexHandler::build_pex_message(&duplicate_peers, &[]);
    let encoded = bencode_msg.encode();

    let local_addr = PeerAddr::new("127.0.0.1", 6890);
    let (added, _dropped) = PexHandler::process_received_pex(&encoded, &local_addr).unwrap();

    assert_eq!(
        added.len(),
        3,
        "Deduplication should reduce to 3 unique peers"
    );

    let unique_ips: Vec<&str> = added.iter().map(|p| p.ip.as_str()).collect();
    assert!(
        unique_ips.contains(&"192.168.1.1"),
        "Should contain 192.168.1.1"
    );
    assert!(unique_ips.contains(&"10.0.0.1"), "Should contain 10.0.0.1");
    assert!(
        unique_ips.contains(&"172.16.0.1"),
        "Should contain 172.16.0.1"
    );
}

#[test]
fn test_mse_encrypted_pex_roundtrip() {
    use crate::bittorrent::extension::mse_crypto::{MseCryptoState, MseDerivedKeys};

    let original_payload = b"d5:added12:....6:added.f2:..e";
    let secret = [0xAAu8; 96];
    let info_hash = [0xBBu8; 20];

    let keys = MseDerivedKeys::derive(&secret, &info_hash);
    let mut sender_crypto = MseCryptoState::new_encrypted(&keys, true);
    let mut receiver_crypto = MseCryptoState::new_encrypted(&keys, false);

    let encrypted = PexHandler::encrypt_payload(original_payload, &mut sender_crypto)
        .expect("Encryption should succeed");
    assert_ne!(
        encrypted,
        original_payload.to_vec(),
        "Encrypted payload should differ from original"
    );

    let decrypted = PexHandler::decrypt_payload(&encrypted, &mut receiver_crypto)
        .expect("Decryption should succeed");
    assert_eq!(
        decrypted,
        original_payload.to_vec(),
        "Decrypted payload should match original"
    );
}

#[test]
fn test_pex_integration_flow_seeder_advertises_ut_pex() {
    let seeder_known_peers = vec![
        PeerAddr::new("192.168.100.1", 6881),
        PeerAddr::new("192.168.100.2", 6882),
        PeerAddr::new("192.168.100.3", 6883),
    ];

    let client_addr = PeerAddr::new("10.0.0.50", 6890);

    let _local_ext_ids = [None, Some(1), Some(2)];
    let remote_ext_ids = vec![Some(1), None, Some(3)];

    assert!(
        PexHandler::is_supported_by_peer(&remote_ext_ids),
        "Remote should support ut_pex (ID=1)"
    );

    let pex_to_send = PexHandler::build_pex_added(&seeder_known_peers, &client_addr, 50);
    let encoded_send = pex_to_send.encode();

    let (discovered_peers, _dropped) =
        PexHandler::process_received_pex(&encoded_send, &client_addr).unwrap();

    assert_eq!(
        discovered_peers.len(),
        3,
        "Client should discover 3 peers from seeder's PEX"
    );
    assert!(
        !discovered_peers.contains(&client_addr),
        "Discovered peers should not include client's own address"
    );

    let client_response_peers = vec![
        PeerAddr::new("10.0.0.51", 6891),
        PeerAddr::new("10.0.0.52", 6892),
    ];
    let client_pex_response = PexHandler::build_pex_added(
        &client_response_peers,
        &PeerAddr::new("192.168.100.1", 6881),
        50,
    );
    let _encoded_response = client_pex_response.encode();

    info!(
        "[PEX Integration Test] Full flow completed: seeder advertised ut_pex, \
         client built/sent PEX, received PEX back, discovered {} peers",
        discovered_peers.len()
    );
}

#[test]
fn test_build_pex_message_separates_v4_v6_keys() {
    // BEP 11 requires IPv4 and IPv6 peers in separate dict keys
    let mixed = vec![
        PeerAddr::new("192.168.1.1", 6881),
        PeerAddr::new("2001:db8::1", 6882),
        PeerAddr::new("10.0.0.1", 6883),
    ];
    let dropped = vec![
        PeerAddr::new("172.16.0.1", 6884),
        PeerAddr::new("fe80::1", 6885),
    ];

    let msg = PexHandler::build_pex_message(&mixed, &dropped);

    // IPv4 added peers should be in "added" key
    let added_bytes = msg.dict_get("added").and_then(|v| v.as_bytes()).unwrap();
    assert_eq!(added_bytes.len(), 2 * 6, "2 IPv4 added peers = 12 bytes");

    // IPv6 added peers should be in "added6" key
    let added6_bytes = msg.dict_get("added6").and_then(|v| v.as_bytes()).unwrap();
    assert_eq!(added6_bytes.len(), 18, "1 IPv6 added peer = 18 bytes");

    // IPv4 dropped peers should be in "dropped" key
    let dropped_bytes = msg.dict_get("dropped").and_then(|v| v.as_bytes()).unwrap();
    assert_eq!(dropped_bytes.len(), 6, "1 IPv4 dropped peer = 6 bytes");

    // IPv6 dropped peers should be in "dropped6" key
    let dropped6_bytes = msg.dict_get("dropped6").and_then(|v| v.as_bytes()).unwrap();
    assert_eq!(dropped6_bytes.len(), 18, "1 IPv6 dropped peer = 18 bytes");

    // Full roundtrip: parse the encoded message
    let encoded = msg.encode();
    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added { peers, dropped } => {
            assert_eq!(peers.len(), 3, "3 added peers total (2 v4 + 1 v6)");
            assert_eq!(dropped.len(), 2, "2 dropped peers total (1 v4 + 1 v6)");
        }
        _ => panic!("Expected Added message"),
    }
}

#[test]
fn test_pex_flags_encoding_preserved() {
    let peer1 = PeerAddr::new("1.2.3.4", 6881);
    let peer2 = PeerAddr::new("5.6.7.8", 6882);
    let peer3 = PeerAddr::new("9.10.11.12", 6883);

    let mut dict = BTreeMap::new();
    dict.insert(
        b"added".to_vec(),
        BencodeValue::Bytes(encode_compact_peers_v4(&[
            peer1.clone(),
            peer2.clone(),
            peer3.clone(),
        ])),
    );
    dict.insert(
        b"added.f".to_vec(),
        BencodeValue::Bytes(vec![0x03, 0x01, 0x02]),
    );

    let value = BencodeValue::Dict(dict);
    let encoded = value.encode();

    let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
    match parsed {
        PexMessage::Added { peers, .. } => {
            assert_eq!(peers.len(), 3, "Should have 3 peers with flags");

            assert_eq!(
                peers[0].flags, 0x03,
                "Peer 1 flags should be 0x03 (encryption + seed)"
            );
            assert_eq!(peers[1].flags, 0x01, "Peer 2 flags should be 0x01");
            assert_eq!(peers[2].flags, 0x02, "Peer 3 flags should be 0x02");

            assert_eq!(peers[0].addr.ip, "1.2.3.4", "Peer 1 IP should match");
            assert_eq!(peers[1].addr.ip, "5.6.7.8", "Peer 2 IP should match");
            assert_eq!(peers[2].addr.ip, "9.10.11.12", "Peer 3 IP should match");
        }
        _ => panic!("Expected Added message with flags"),
    }
}
