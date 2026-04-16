use std::collections::BTreeMap;

use crate::bittorrent::bencode::codec::BencodeValue;
use crate::bittorrent::extension::mse_crypto::MseCryptoState;
use crate::bittorrent::peer::connection::PeerAddr;

#[derive(Debug, Clone)]
pub struct PexAddedPeer {
    pub addr: PeerAddr,
    pub flags: u8,
}

#[derive(Debug, Clone)]
pub enum PexMessage {
    Added { peers: Vec<PexAddedPeer> },
    Removed(Vec<PeerAddr>),
}

pub struct PexHandler;

impl PexHandler {
    pub const EXTENSION_NAME: &'static str = "ut_pex";
    pub const EXTENSION_ID: u8 = 1;
    const COMPACT_PEER_SIZE_V4: usize = 6;
    const COMPACT_PEER_SIZE_V6: usize = 18;
    pub const DEFAULT_MAX_PEERS: usize = 50; // Used as default for build_pex_added() max_peers param

    pub fn parse_pex_data(data: &[u8]) -> Result<PexMessage, String> {
        let (value, _) = BencodeValue::decode(data)
            .map_err(|e| format!("Failed to decode PEX bencode: {}", e))?;

        if !value.is_dict() {
            return Err("PEX message must be a bencoded dictionary".to_string());
        }

        let mut added_peers = Vec::new();
        if let Some(added_data) = value.dict_get("added").and_then(|v| v.as_bytes()) {
            added_peers = decode_compact_peers(added_data)?;
        }

        let mut flags = Vec::new();
        if let Some(flags_data) = value.dict_get("added.f").and_then(|v| v.as_bytes()) {
            flags = flags_data.to_vec();
        }

        let _removed_peers =
            if let Some(dropped_data) = value.dict_get("dropped").and_then(|v| v.as_bytes()) {
                decode_compact_peers(dropped_data)?
            } else {
                Vec::new()
            };

        let peers_with_flags: Vec<PexAddedPeer> = added_peers
            .into_iter()
            .enumerate()
            .map(|(i, addr)| PexAddedPeer {
                addr,
                flags: flags.get(i).copied().unwrap_or(0),
            })
            .collect();

        Ok(PexMessage::Added {
            peers: peers_with_flags,
        })
    }

    pub fn build_pex_message(added: &[PeerAddr], removed: &[PeerAddr]) -> BencodeValue {
        let mut dict = BTreeMap::new();

        if !added.is_empty() {
            let compact_added = encode_compact_peers(added);
            let flag_bytes = vec![0u8; added.len()];
            dict.insert(b"added".to_vec(), BencodeValue::Bytes(compact_added));
            dict.insert(b"added.f".to_vec(), BencodeValue::Bytes(flag_bytes));
        }

        if !removed.is_empty() {
            let compact_removed = encode_compact_peers(removed);
            dict.insert(b"dropped".to_vec(), BencodeValue::Bytes(compact_removed));
        }

        BencodeValue::Dict(dict)
    }

    pub fn is_supported_by_peer(extension_ids: &[Option<u8>]) -> bool {
        extension_ids.contains(&Some(Self::EXTENSION_ID))
    }

    pub fn build_pex_added(
        known_peers: &[PeerAddr],
        remote_addr: &PeerAddr,
        max_peers: usize,
    ) -> BencodeValue {
        let filtered: Vec<PeerAddr> = known_peers
            .iter()
            .filter(|peer| **peer != *remote_addr)
            .take(max_peers)
            .cloned()
            .collect();

        Self::build_pex_message(&filtered, &[])
    }

    pub fn process_received_pex(
        data: &[u8],
        local_addr: &PeerAddr,
    ) -> Result<(Vec<PeerAddr>, Vec<PeerAddr>), String> {
        let msg = Self::parse_pex_data(data)?;

        match msg {
            PexMessage::Added { peers } => {
                let added: Vec<PeerAddr> = peers
                    .into_iter()
                    .map(|p| p.addr)
                    .filter(|addr| addr != local_addr)
                    .collect();

                let added_deduped = deduplicate_peers(&added);
                Ok((added_deduped, vec![]))
            }
            PexMessage::Removed(peers) => {
                let filtered: Vec<PeerAddr> = peers
                    .into_iter()
                    .filter(|addr| addr != local_addr)
                    .collect();
                let deduped = deduplicate_peers(&filtered);
                Ok((vec![], deduped))
            }
        }
    }

    /// Encrypt outgoing PEX payload using MSE stream cipher (RC4)
    ///
    /// Uses the existing MseCryptoState from encrypted_connection.rs to encrypt
    /// PEX messages when MSE negotiation has been completed with a peer.
    ///
    /// # Arguments
    /// * `payload` - The raw PEX bencoded message bytes to encrypt
    /// * `cipher` - Mutable reference to the MSE crypto state (contains RC4 cipher)
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Encrypted payload ready for transmission
    /// * `Err(String)` - Encryption failure description
    pub fn encrypt_payload(payload: &[u8], cipher: &mut MseCryptoState) -> Result<Vec<u8>, String> {
        if !cipher.is_encrypted() {
            return Ok(payload.to_vec());
        }

        let mut encrypted = payload.to_vec();
        cipher.encrypt(&mut encrypted);
        Ok(encrypted)
    }

    /// Decrypt incoming PEX payload using MSE stream cipher (RC4)
    ///
    /// Uses the existing MseCryptoState to decrypt PEX messages received
    /// from peers that have completed MSE handshake.
    ///
    /// # Arguments
    /// * `encrypted` - The encrypted PEX payload received from peer
    /// * `cipher` - Mutable reference to the MSE crypto state (contains RC4 cipher)
    ///
    /// # Returns
    /// * `Ok(Vec<u8>)` - Decrypted PEX payload ready for parsing
    /// * `Err(String)` - Decryption failure description
    pub fn decrypt_payload(
        encrypted: &[u8],
        cipher: &mut MseCryptoState,
    ) -> Result<Vec<u8>, String> {
        if !cipher.is_encrypted() {
            return Ok(encrypted.to_vec());
        }

        let mut decrypted = encrypted.to_vec();
        cipher.decrypt(&mut decrypted);
        Ok(decrypted)
    }
}

fn encode_compact_peers(peers: &[PeerAddr]) -> Vec<u8> {
    let mut result = Vec::new();

    for peer in peers {
        if let Ok(ipv4) = peer.ip.parse::<std::net::Ipv4Addr>() {
            let mut buf = [0u8; 6];
            buf[..4].copy_from_slice(&ipv4.octets());
            buf[4..6].copy_from_slice(&peer.port.to_be_bytes());
            result.extend_from_slice(&buf);
        } else if let Ok(ipv6) = peer.ip.parse::<std::net::Ipv6Addr>() {
            let mut buf = [0u8; 18];
            buf[..16].copy_from_slice(&ipv6.octets());
            buf[16..18].copy_from_slice(&peer.port.to_be_bytes());
            result.extend_from_slice(&buf);
        }
    }

    result
}

fn decode_compact_peers(data: &[u8]) -> Result<Vec<PeerAddr>, String> {
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let is_v4 = data.len().is_multiple_of(PexHandler::COMPACT_PEER_SIZE_V4);
    let is_v6 = data.len().is_multiple_of(PexHandler::COMPACT_PEER_SIZE_V6)
        && data.len() >= PexHandler::COMPACT_PEER_SIZE_V6;

    if !is_v4 && !is_v6 {
        return Err(format!(
            "Invalid compact peer data length: {} (must be multiple of {} for IPv4 or {} for IPv6)",
            data.len(),
            PexHandler::COMPACT_PEER_SIZE_V4,
            PexHandler::COMPACT_PEER_SIZE_V6
        ));
    }

    let peer_size = if is_v6 && !is_v4 {
        PexHandler::COMPACT_PEER_SIZE_V6
    } else {
        PexHandler::COMPACT_PEER_SIZE_V4
    };

    let peer_count = data.len() / peer_size;
    let mut peers = Vec::with_capacity(peer_count);

    for i in 0..peer_count {
        let start = i * peer_size;
        let end = start + peer_size;

        if peer_size == PexHandler::COMPACT_PEER_SIZE_V6 {
            let peer = decode_ipv6_peer(&data[start..end])
                .ok_or_else(|| format!("Failed to parse IPv6 peer at index {}", i))?;
            peers.push(peer);
        } else {
            let peer = PeerAddr::from_compact(&data[start..end])
                .ok_or_else(|| format!("Failed to parse IPv4 peer at index {}", i))?;
            peers.push(peer);
        }
    }

    Ok(peers)
}

fn decode_ipv6_peer(data: &[u8]) -> Option<PeerAddr> {
    if data.len() < 18 {
        return None;
    }

    let ip_bytes: [u8; 16] = data[..16].try_into().ok()?;
    let ipv6 = std::net::Ipv6Addr::from(ip_bytes);
    let port = u16::from_be_bytes([data[16], data[17]]);

    Some(PeerAddr {
        ip: ipv6.to_string(),
        port,
    })
}

fn deduplicate_peers(peers: &[PeerAddr]) -> Vec<PeerAddr> {
    let mut seen = std::collections::HashSet::new();
    peers
        .iter()
        .filter(|peer| seen.insert((peer.ip.clone(), peer.port)))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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
            PexMessage::Added { peers } => {
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

        let encoded = encode_compact_peers(&peers);
        assert_eq!(encoded.len(), peers.len() * 6);

        let decoded = decode_compact_peers(&encoded).unwrap();
        assert_eq!(decoded.len(), peers.len());

        for i in 0..peers.len() {
            assert_eq!(decoded[i].ip, peers[i].ip);
            assert_eq!(decoded[i].port, peers[i].port);
        }
    }

    #[test]
    fn test_encode_decode_compact_peers_v6() {
        let peers = vec![PeerAddr::new("2001:db8::1", 6881)];

        let encoded = encode_compact_peers(&peers);
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
        let decoded = decode_compact_peers(added_data).unwrap();

        assert!(!decoded.contains(&remote_addr));
        assert_eq!(decoded.len(), 2);
    }

    #[test]
    fn test_parse_pex_with_flags() {
        use std::collections::BTreeMap;

        let mut dict = BTreeMap::new();
        let peer1 = PeerAddr::new("1.2.3.4", 6881);
        let peer2 = PeerAddr::new("5.6.7.8", 6882);

        dict.insert(
            b"added".to_vec(),
            BencodeValue::Bytes(encode_compact_peers(&[peer1.clone(), peer2.clone()])),
        );
        dict.insert(b"added.f".to_vec(), BencodeValue::Bytes(vec![0x03, 0x01]));

        let value = BencodeValue::Dict(dict);
        let encoded = value.encode();

        let parsed = PexHandler::parse_pex_data(&encoded).unwrap();
        match parsed {
            PexMessage::Added { peers } => {
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
            PexMessage::Added { peers } => {
                assert!(peers.is_empty());
            }
            _ => panic!("Expected Added message"),
        }
    }

    // ==================== Task H6: 6 Comprehensive PEX Tests ====================

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
            PexMessage::Added { peers } => {
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
            PexMessage::Added { peers } => {
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
        let secret = b"pex_encryption_test_secret_key";

        let keys = MseDerivedKeys::derive(secret);
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
    fn test_pex_flags_encoding_preserved() {
        use std::collections::BTreeMap;

        let peer1 = PeerAddr::new("1.2.3.4", 6881);
        let peer2 = PeerAddr::new("5.6.7.8", 6882);
        let peer3 = PeerAddr::new("9.10.11.12", 6883);

        let mut dict = BTreeMap::new();
        dict.insert(
            b"added".to_vec(),
            BencodeValue::Bytes(encode_compact_peers(&[
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
            PexMessage::Added { peers } => {
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
}
