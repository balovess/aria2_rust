//! Tests for extension registry integration and extended message dispatch.

use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
use aria2_protocol::bittorrent::message::types::BtMessage;

use crate::engine::extension_registry::ExtensionUpdate;
use crate::engine::bt_peer_interaction::BtPeerInteractive;

use super::make_test_conn;

#[test]
fn test_extension_registry_initial_state() {
    let info_hash = [0u8; 20];
    let interactive = BtPeerInteractive::new(info_hash, 100);
    assert_eq!(interactive.extension_registry().local_ut_metadata_id(), 1);
    assert_eq!(interactive.extension_registry().local_ut_pex_id(), 2);
    assert!(
        interactive
            .extension_registry()
            .peer_ut_metadata_id()
            .is_none()
    );
    assert!(interactive.extension_registry().peer_ut_pex_id().is_none());
}

#[test]
fn test_dispatch_extended_handshake() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // Build and dispatch an extension handshake message
    let hs = ExtensionHandshake::new();
    let payload = hs.to_bytes();
    let msg = BtMessage::Extended { ext_id: 0, payload };

    let update = interactive.dispatch_message(msg, &mut conn, |_| false);

    // Verify the extension update was produced
    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
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

    // Verify the registry was updated
    assert_eq!(
        interactive.extension_registry().peer_ut_metadata_id(),
        Some(1)
    );
    assert_eq!(interactive.extension_registry().peer_ut_pex_id(), Some(2));

    // PEX should be auto-enabled after handshake
    assert!(interactive.ut_pex_enabled);
}

#[test]
fn test_dispatch_extended_ut_metadata_request() {
    use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // First, receive a handshake so the registry knows the peer's IDs
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // Now dispatch a ut_metadata request (peer's id = 1)
    let msg = UtMetadataMessage::Request { piece: 0 };
    let payload = msg.to_payload();
    let update = interactive.dispatch_message(
        BtMessage::Extended { ext_id: 1, payload },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
        ExtensionUpdate::MetadataRequest { piece } => {
            assert_eq!(piece, 0);
        }
        other => panic!("Expected MetadataRequest, got {:?}", other),
    }
}

#[test]
fn test_dispatch_extended_ut_metadata_data() {
    use aria2_protocol::bittorrent::message::extension::UtMetadataMessage;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    let msg = UtMetadataMessage::Data {
        piece: 2,
        total_size: 50000,
        data: b"test metadata".to_vec(),
    };
    let payload = msg.to_payload();
    let update = interactive.dispatch_message(
        BtMessage::Extended { ext_id: 1, payload },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
        ExtensionUpdate::MetadataPiece {
            piece,
            total_size,
            data,
        } => {
            assert_eq!(piece, 2);
            assert_eq!(total_size, 50000);
            assert_eq!(data, b"test metadata");
        }
        other => panic!("Expected MetadataPiece, got {:?}", other),
    }
}

#[test]
fn test_dispatch_extended_ut_pex() {
    use aria2_protocol::bittorrent::message::extension::{CompactPeerV4, UtPexMessage};

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // Build a PEX message
    let mut pex = UtPexMessage::new();
    let mut peer_bytes = [0u8; 6];
    peer_bytes[..4].copy_from_slice(&[10, 0, 0, 1]);
    peer_bytes[4..6].copy_from_slice(&6881u16.to_be_bytes());
    pex.added.push(CompactPeerV4(peer_bytes));

    let payload = pex.to_payload();
    // Peer's ut_pex id = 2
    let update = interactive.dispatch_message(
        BtMessage::Extended { ext_id: 2, payload },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_some());
    match update.extension_update.unwrap() {
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
            assert_eq!(added_v4[0], CompactPeerV4(peer_bytes));
        }
        other => panic!("Expected PeerExchange, got {:?}", other),
    }
}

#[test]
fn test_dispatch_extended_unknown_ext_id() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // Receive handshake first
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // Dispatch with unknown ext_id
    let update = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 99,
            payload: vec![],
        },
        &mut conn,
        |_| false,
    );

    assert!(update.extension_update.is_none());
}

#[test]
fn test_dispatch_extended_handshake_enables_pex() {
    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // PEX should be disabled initially
    assert!(!interactive.ut_pex_enabled);

    // Receive handshake that includes ut_pex
    let hs = ExtensionHandshake::new();
    let hs_payload = hs.to_bytes();
    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: hs_payload,
        },
        &mut conn,
        |_| false,
    );

    // PEX should now be enabled
    assert!(interactive.ut_pex_enabled);
}

#[test]
fn test_dispatch_extended_handshake_without_pex() {
    use aria2_protocol::bittorrent::bencode::codec::BencodeValue;
    use std::collections::BTreeMap;

    let info_hash = [0u8; 20];
    let mut interactive = BtPeerInteractive::new(info_hash, 100);
    let mut conn = make_test_conn();

    // Build a handshake with only ut_metadata (no ut_pex)
    let mut m_dict = BTreeMap::new();
    m_dict.insert(b"ut_metadata".to_vec(), BencodeValue::Int(1));
    let mut root = BTreeMap::new();
    root.insert(b"m".to_vec(), BencodeValue::Dict(m_dict));
    root.insert(b"reqq".to_vec(), BencodeValue::Int(500));
    let bytes = BencodeValue::Dict(root).encode();

    let _ = interactive.dispatch_message(
        BtMessage::Extended {
            ext_id: 0,
            payload: bytes,
        },
        &mut conn,
        |_| false,
    );

    // PEX should remain disabled since peer doesn't support it
    assert!(!interactive.ut_pex_enabled);
    // But ut_metadata should be available
    assert!(interactive.extension_registry().supports_ut_metadata());
}
