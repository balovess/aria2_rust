//! Tests for the extension registry and dispatch logic.

use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
use aria2_protocol::bittorrent::message::extension::{
    CompactPeerV4, ExtensionHandshake, UtMetadataMessage, UtPexMessage,
};
use std::collections::BTreeMap;

use super::{
    dispatch_extension_message, ExtensionRegistry, ExtensionUpdate, DEFAULT_REQQ,
    UT_METADATA_NAME, UT_PEX_NAME,
};

// ====================== ExtensionRegistry tests ======================

#[test]
fn test_registry_new_default_local_ids() {
    let reg = ExtensionRegistry::new();
    assert_eq!(reg.local_ut_metadata_id(), 1);
    assert_eq!(reg.local_ut_pex_id(), 2);
}

#[test]
fn test_registry_new_peer_ids_initially_none() {
    let reg = ExtensionRegistry::new();
    assert!(reg.peer_ut_metadata_id().is_none());
    assert!(reg.peer_ut_pex_id().is_none());
}

#[test]
fn test_registry_default_trait() {
    let reg = ExtensionRegistry::default();
    assert_eq!(reg.local_ut_metadata_id(), 1);
    assert_eq!(reg.local_ut_pex_id(), 2);
}

#[test]
fn test_registry_default_reqq() {
    let reg = ExtensionRegistry::new();
    assert_eq!(reg.reqq(), DEFAULT_REQQ);
}

#[test]
fn test_update_from_peer_handshake_full() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new(); // ut_metadata=1, ut_pex=2, reqq=500
    reg.update_from_peer_handshake(&hs);

    assert_eq!(reg.peer_ut_metadata_id(), Some(1));
    assert_eq!(reg.peer_ut_pex_id(), Some(2));
    assert_eq!(reg.reqq(), 500);
}

#[test]
fn test_update_from_peer_handshake_custom_ids() {
    let mut reg = ExtensionRegistry::new();
    let mut hs = ExtensionHandshake::new();
    hs.with_ut_metadata(3).with_ut_pex(5).with_reqq(1000);
    reg.update_from_peer_handshake(&hs);

    assert_eq!(reg.peer_ut_metadata_id(), Some(3));
    assert_eq!(reg.peer_ut_pex_id(), Some(5));
    assert_eq!(reg.reqq(), 1000);
}

#[test]
fn test_update_from_peer_handshake_missing_ut_metadata() {
    let mut reg = ExtensionRegistry::new();
    // Build a handshake with only ut_pex
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_pex".to_vec(), BencodeValue::Int(2));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
    let bytes = BencodeValue::Dict(root).encode();
    let hs = ExtensionHandshake::from_bytes(&bytes).unwrap();

    reg.update_from_peer_handshake(&hs);
    assert!(reg.peer_ut_metadata_id().is_none());
    assert_eq!(reg.peer_ut_pex_id(), Some(2));
}

#[test]
fn test_update_from_peer_handshake_missing_ut_pex() {
    let mut reg = ExtensionRegistry::new();
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
    let bytes = BencodeValue::Dict(root).encode();
    let hs = ExtensionHandshake::from_bytes(&bytes).unwrap();

    reg.update_from_peer_handshake(&hs);
    assert_eq!(reg.peer_ut_metadata_id(), Some(1));
    assert!(reg.peer_ut_pex_id().is_none());
}

#[test]
fn test_is_extension_enabled() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    // ut_metadata has peer id 1, ut_pex has peer id 2
    assert!(reg.is_extension_enabled(1));
    assert!(reg.is_extension_enabled(2));
    assert!(!reg.is_extension_enabled(3));
    assert!(!reg.is_extension_enabled(0));
}

#[test]
fn test_extension_name_for_id() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    assert_eq!(reg.extension_name_for_id(1), Some(&UT_METADATA_NAME[..]));
    assert_eq!(reg.extension_name_for_id(2), Some(&UT_PEX_NAME[..]));
    assert_eq!(reg.extension_name_for_id(3), None);
}

#[test]
fn test_supports_ut_metadata_and_pex() {
    let mut reg = ExtensionRegistry::new();
    assert!(!reg.supports_ut_metadata());
    assert!(!reg.supports_ut_pex());

    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);
    assert!(reg.supports_ut_metadata());
    assert!(reg.supports_ut_pex());
}

#[test]
fn test_build_local_handshake() {
    let reg = ExtensionRegistry::new();
    let hs = reg.build_local_handshake();
    assert_eq!(hs.ut_metadata_id(), Some(1));
    assert_eq!(hs.ut_pex_id(), Some(2));
    assert_eq!(hs.reqq(), DEFAULT_REQQ);
}

#[test]
fn test_update_from_peer_handshake_overwrites() {
    let mut reg = ExtensionRegistry::new();

    // First handshake
    let mut hs1 = ExtensionHandshake::new();
    hs1.with_ut_metadata(3).with_ut_pex(4);
    reg.update_from_peer_handshake(&hs1);
    assert_eq!(reg.peer_ut_metadata_id(), Some(3));
    assert_eq!(reg.peer_ut_pex_id(), Some(4));

    // Second handshake (should overwrite)
    let mut hs2 = ExtensionHandshake::new();
    hs2.with_ut_metadata(7).with_ut_pex(8);
    reg.update_from_peer_handshake(&hs2);
    assert_eq!(reg.peer_ut_metadata_id(), Some(7));
    assert_eq!(reg.peer_ut_pex_id(), Some(8));
}

// ====================== dispatch_extension_message tests ======================

#[test]
fn test_dispatch_handshake() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    let payload = hs.to_bytes();

    let result = dispatch_extension_message(&mut reg, 0, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::HandshakeReceived {
            ut_metadata_id,
            ut_pex_id,
            reqq,
        } => {
            assert_eq!(ut_metadata_id, Some(1));
            assert_eq!(ut_pex_id, Some(2));
            assert_eq!(reqq, 500);
        }
        other => panic!("Expected HandshakeReceived, got {:?}", other),
    }

    // Registry should now be updated
    assert_eq!(reg.peer_ut_metadata_id(), Some(1));
    assert_eq!(reg.peer_ut_pex_id(), Some(2));
}

#[test]
fn test_dispatch_handshake_custom_ids() {
    let mut reg = ExtensionRegistry::new();
    let mut hs = ExtensionHandshake::new();
    hs.with_ut_metadata(5).with_ut_pex(6).with_reqq(200);
    let payload = hs.to_bytes();

    let result = dispatch_extension_message(&mut reg, 0, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::HandshakeReceived {
            ut_metadata_id,
            ut_pex_id,
            reqq,
        } => {
            assert_eq!(ut_metadata_id, Some(5));
            assert_eq!(ut_pex_id, Some(6));
            assert_eq!(reqq, 200);
        }
        other => panic!("Expected HandshakeReceived, got {:?}", other),
    }
}

#[test]
fn test_dispatch_handshake_invalid_payload() {
    let mut reg = ExtensionRegistry::new();
    let result = dispatch_extension_message(&mut reg, 0, b"not bencoded");
    assert!(result.is_none());
}

#[test]
fn test_dispatch_ut_metadata_request() {
    let mut reg = ExtensionRegistry::new();
    // First, receive a handshake so the registry knows peer's ut_metadata id
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    // Build a ut_metadata request payload
    let msg = UtMetadataMessage::Request { piece: 0 };
    let payload = msg.to_payload();

    // Peer's ut_metadata id is 1
    let result = dispatch_extension_message(&mut reg, 1, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::MetadataRequest { piece } => {
            assert_eq!(piece, 0);
        }
        other => panic!("Expected MetadataRequest, got {:?}", other),
    }
}

#[test]
fn test_dispatch_ut_metadata_data() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    let metadata = b"hello world".to_vec();
    let msg = UtMetadataMessage::Data {
        piece: 2,
        total_size: 1000,
        data: metadata,
    };
    let payload = msg.to_payload();

    let result = dispatch_extension_message(&mut reg, 1, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::MetadataPiece {
            piece,
            total_size,
            data,
        } => {
            assert_eq!(piece, 2);
            assert_eq!(total_size, 1000);
            assert_eq!(data, b"hello world");
        }
        other => panic!("Expected MetadataPiece, got {:?}", other),
    }
}

#[test]
fn test_dispatch_ut_metadata_reject() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    let msg = UtMetadataMessage::Reject { piece: 7 };
    let payload = msg.to_payload();

    let result = dispatch_extension_message(&mut reg, 1, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::MetadataReject { piece } => {
            assert_eq!(piece, 7);
        }
        other => panic!("Expected MetadataReject, got {:?}", other),
    }
}

#[test]
fn test_dispatch_ut_pex() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    // Build a PEX message
    let mut pex = UtPexMessage::new();
    let mut peer1 = [0u8; 6];
    peer1[..4].copy_from_slice(&[192, 168, 1, 1]);
    peer1[4..6].copy_from_slice(&6881u16.to_be_bytes());
    pex.added.push(CompactPeerV4(peer1));

    let payload = pex.to_payload();

    // Peer's ut_pex id is 2
    let result = dispatch_extension_message(&mut reg, 2, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::PeerExchange {
            added_v4,
            added_v6,
            dropped_v4,
            dropped_v6,
        } => {
            assert_eq!(added_v4.len(), 1);
            assert!(added_v6.is_empty());
            assert!(dropped_v4.is_empty());
            assert!(dropped_v6.is_empty());
            assert_eq!(added_v4[0], CompactPeerV4(peer1));
        }
        other => panic!("Expected PeerExchange, got {:?}", other),
    }
}

#[test]
fn test_dispatch_unknown_ext_id() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    // ext_id 99 is not assigned to any extension
    let result = dispatch_extension_message(&mut reg, 99, &[]);
    assert!(result.is_none());
}

#[test]
fn test_dispatch_no_peer_handshake_yet() {
    let mut reg = ExtensionRegistry::new();
    // No handshake received yet, so peer IDs are None

    // ext_id 1 with some payload — unknown because no handshake yet
    let result = dispatch_extension_message(&mut reg, 1, &[]);
    assert!(result.is_none());
}

#[test]
fn test_dispatch_ut_metadata_with_custom_peer_id() {
    let mut reg = ExtensionRegistry::new();
    // Peer uses ut_metadata=5, ut_pex=6
    let mut hs = ExtensionHandshake::new();
    hs.with_ut_metadata(5).with_ut_pex(6);
    reg.update_from_peer_handshake(&hs);

    let msg = UtMetadataMessage::Request { piece: 3 };
    let payload = msg.to_payload();

    // Must use peer's id (5), not our local id (1)
    let result = dispatch_extension_message(&mut reg, 5, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::MetadataRequest { piece } => {
            assert_eq!(piece, 3);
        }
        other => panic!("Expected MetadataRequest, got {:?}", other),
    }

    // Using our local id (1) should not match
    let result2 = dispatch_extension_message(&mut reg, 1, &payload);
    assert!(result2.is_none());
}

#[test]
fn test_dispatch_ut_pex_with_custom_peer_id() {
    let mut reg = ExtensionRegistry::new();
    let mut hs = ExtensionHandshake::new();
    hs.with_ut_metadata(5).with_ut_pex(6);
    reg.update_from_peer_handshake(&hs);

    let pex = UtPexMessage::new();
    let payload = pex.to_payload();

    // Must use peer's id (6)
    let result = dispatch_extension_message(&mut reg, 6, &payload);
    assert!(result.is_some());

    match result.unwrap() {
        ExtensionUpdate::PeerExchange {
            added_v4,
            added_v6,
            dropped_v4,
            dropped_v6,
        } => {
            assert!(added_v4.is_empty());
            assert!(added_v6.is_empty());
            assert!(dropped_v4.is_empty());
            assert!(dropped_v6.is_empty());
        }
        other => panic!("Expected PeerExchange, got {:?}", other),
    }
}

#[test]
fn test_dispatch_invalid_ut_metadata_payload() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    let result = dispatch_extension_message(&mut reg, 1, b"garbage");
    assert!(result.is_none());
}

#[test]
fn test_dispatch_invalid_ut_pex_payload() {
    let mut reg = ExtensionRegistry::new();
    let hs = ExtensionHandshake::new();
    reg.update_from_peer_handshake(&hs);

    // Invalid compact peer data length (5 bytes, not multiple of 6)
    let mut dict = BTreeMap::new();
    dict.insert(b"added".to_vec(), BencodeValue::Bytes(vec![1, 2, 3, 4, 5]));
    let payload = BencodeValue::Dict(dict).encode();

    let result = dispatch_extension_message(&mut reg, 2, &payload);
    assert!(result.is_none());
}

#[test]
fn test_dispatch_full_roundtrip_handshake_then_metadata() {
    let mut reg = ExtensionRegistry::new();

    // Step 1: Receive handshake
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let result = dispatch_extension_message(&mut reg, 0, &hs_payload);
    assert!(result.is_some());

    // Step 2: Receive ut_metadata request
    let msg = UtMetadataMessage::Request { piece: 0 };
    let meta_payload = msg.to_payload();
    let result = dispatch_extension_message(&mut reg, 1, &meta_payload);
    assert!(result.is_some());

    // Step 3: Receive ut_pex message
    let pex = UtPexMessage::new();
    let pex_payload = pex.to_payload();
    let result = dispatch_extension_message(&mut reg, 2, &pex_payload);
    assert!(result.is_some());
}

#[test]
fn test_extension_update_debug_format() {
    let update = ExtensionUpdate::HandshakeReceived {
        ut_metadata_id: Some(1),
        ut_pex_id: Some(2),
        reqq: 500,
    };
    let s = format!("{:?}", update);
    assert!(s.contains("HandshakeReceived"));

    let update = ExtensionUpdate::MetadataPiece {
        piece: 0,
        total_size: 1000,
        data: vec![1, 2, 3],
    };
    let s = format!("{:?}", update);
    assert!(s.contains("MetadataPiece"));

    let update = ExtensionUpdate::MetadataRequest { piece: 5 };
    let s = format!("{:?}", update);
    assert!(s.contains("MetadataRequest"));

    let update = ExtensionUpdate::MetadataReject { piece: 5 };
    let s = format!("{:?}", update);
    assert!(s.contains("MetadataReject"));

    let update = ExtensionUpdate::PeerExchange {
        added_v4: Vec::new(),
        added_v6: Vec::new(),
        dropped_v4: Vec::new(),
        dropped_v6: Vec::new(),
    };
    let s = format!("{:?}", update);
    assert!(s.contains("PeerExchange"));
}

#[test]
fn test_registry_roundtrip_with_handshake() {
    let reg = ExtensionRegistry::new();
    let hs = reg.build_local_handshake();
    let bytes = hs.to_bytes();
    let parsed = ExtensionHandshake::from_bytes(&bytes).unwrap();
    assert_eq!(parsed.ut_metadata_id(), Some(reg.local_ut_metadata_id()));
    assert_eq!(parsed.ut_pex_id(), Some(reg.local_ut_pex_id()));
}
