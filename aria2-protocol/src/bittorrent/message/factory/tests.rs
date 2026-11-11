use super::*;

#[test]
fn test_parse_keepalive() {
    let msg = parse_message(&[0, 0, 0, 0]).unwrap();
    assert_eq!(msg, Some(BtMessage::KeepAlive));
}

#[test]
fn test_parse_choke() {
    let msg = parse_message(&[0, 0, 0, 1, 0]).unwrap();
    assert_eq!(msg, Some(BtMessage::Choke));
}

#[test]
fn test_parse_unchoke() {
    let msg = parse_message(&[0, 0, 0, 1, 1]).unwrap();
    assert_eq!(msg, Some(BtMessage::Unchoke));
}

#[test]
fn test_parse_interested() {
    let msg = parse_message(&[0, 0, 0, 1, 2]).unwrap();
    assert_eq!(msg, Some(BtMessage::Interested));
}

#[test]
fn test_parse_not_interested() {
    let msg = parse_message(&[0, 0, 0, 1, 3]).unwrap();
    assert_eq!(msg, Some(BtMessage::NotInterested));
}

#[test]
fn test_parse_have() {
    let mut data = vec![0, 0, 0, 5, 4];
    data.extend_from_slice(&(99u32).to_be_bytes());
    let msg = parse_message(&data).unwrap();
    assert_eq!(msg, Some(BtMessage::Have { piece_index: 99 }));
}

#[test]
fn test_parse_bitfield() {
    let data = vec![0, 0, 0, 3, 5, 0xFF, 0x00];
    let msg = parse_message(&data).unwrap();
    assert_eq!(
        msg,
        Some(BtMessage::Bitfield {
            data: vec![0xFF, 0x00]
        })
    );
}

#[test]
fn test_parse_request() {
    let mut data = vec![0, 0, 0, 13, 6];
    data.extend_from_slice(&(1u32).to_be_bytes());
    data.extend_from_slice(&(1024u32).to_be_bytes());
    data.extend_from_slice(&(16384u32).to_be_bytes());
    let msg = parse_message(&data).unwrap();
    assert_eq!(
        msg,
        Some(BtMessage::Request {
            request: PieceBlockRequest::new(1, 1024, 16384)
        })
    );
}

#[test]
fn test_parse_piece() {
    let block_data = b"hi";
    let total_len: u32 = 9 + block_data.len() as u32;
    let mut data = total_len.to_be_bytes().to_vec();
    data.push(7);
    data.extend_from_slice(&(0u32).to_be_bytes());
    data.extend_from_slice(&(0u32).to_be_bytes());
    data.extend_from_slice(block_data);
    let msg = parse_message(&data).unwrap();
    assert_eq!(
        msg,
        Some(BtMessage::Piece {
            index: 0,
            begin: 0,
            data: Bytes::from_static(b"hi")
        })
    );
}

#[test]
fn test_parse_piece_bytes_reuses_frame_storage() {
    let frame = Bytes::from_static(&[0, 0, 0, 13, 7, 0, 0, 0, 3, 0, 0, 0, 4, 1, 2, 3, 4]);
    let parsed = parse_message_bytes(frame.clone()).unwrap().unwrap();
    match parsed {
        BtMessage::Piece { index, begin, data } => {
            assert_eq!(index, 3);
            assert_eq!(begin, 4);
            assert_eq!(data, Bytes::from_static(&[1, 2, 3, 4]));
            assert_eq!(data.as_ptr(), unsafe { frame.as_ptr().add(13) });
        }
        other => panic!("expected piece, got {other:?}"),
    }
}

#[test]
fn test_fixed_length_messages_reject_trailing_payload() {
    let mut have = vec![0, 0, 0, 6, 4];
    have.extend_from_slice(&7u32.to_be_bytes());
    have.push(0xFF);
    assert!(parse_message(&have).is_err());

    let mut port = vec![0, 0, 0, 4, 9];
    port.extend_from_slice(&6881u16.to_be_bytes());
    port.push(0xFF);
    assert!(parse_message(&port).is_err());

    assert!(parse_message(&[0, 0, 0, 2, 0, 0xFF]).is_err());
}

#[test]
fn test_piece_requires_non_empty_data() {
    // Message length = ID + index + begin, with no block bytes.
    let frame = [0, 0, 0, 9, 7, 0, 0, 0, 0, 0, 0, 0, 0];
    assert!(parse_message(&frame).is_err());
    assert!(parse_message_bytes(Bytes::copy_from_slice(&frame)).is_err());
}

#[test]
fn test_parse_cancel() {
    let mut data = vec![0, 0, 0, 13, 8];
    data.extend_from_slice(&(5u32).to_be_bytes());
    data.extend_from_slice(&(200u32).to_be_bytes());
    data.extend_from_slice(&(8192u32).to_be_bytes());
    let msg = parse_message(&data).unwrap();
    assert_eq!(
        msg,
        Some(BtMessage::Cancel {
            request: PieceBlockRequest::new(5, 200, 8192)
        })
    );
}

#[test]
fn test_parse_invalid_id() {
    let err = parse_message(&[0, 0, 0, 1, 255]);
    assert!(err.is_err());
}

#[test]
fn test_parse_stream_multiple() {
    let mut stream = vec![];
    stream.extend_from_slice(&[0, 0, 0, 0]);
    stream.extend_from_slice(&[0, 0, 0, 1, 0]);
    stream.extend_from_slice(&[0, 0, 0, 1, 1]);

    let msgs = parse_message_stream(&stream);
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].0, Some(BtMessage::KeepAlive));
    assert_eq!(msgs[1].0, Some(BtMessage::Choke));
    assert_eq!(msgs[2].0, Some(BtMessage::Unchoke));
}

#[test]
fn test_empty_input() {
    assert!(parse_message(&[]).unwrap().is_none());
}

#[test]
fn test_truncated_message() {
    let err = parse_message(&[0, 0, 0, 5, 4, 0, 0]);
    assert!(err.is_err());
}

// --- Phase 13 / Wave A — Task A6: BT Message Unit Tests ---

#[test]
fn test_a6_01_allowed_fast_roundtrip_index_42() {
    use super::super::serializer::serialize_allowed_fast;
    let msg = BtMessage::AllowedFast { index: 42 };
    let bytes = serialize_allowed_fast(42);
    let decoded = parse_message(&bytes).unwrap().unwrap();
    assert_eq!(decoded, msg);
    if let BtMessage::AllowedFast { index } = decoded {
        assert_eq!(index, 42);
    }
}

#[test]
fn test_a6_02_reject_roundtrip() {
    use super::super::serializer::serialize_reject;
    let msg = BtMessage::Reject {
        index: 10,
        offset: 100,
        length: 16384,
    };
    let bytes = serialize_reject(10, 100, 16384);
    let decoded = parse_message(&bytes).unwrap().unwrap();
    assert_eq!(decoded, msg);
    if let BtMessage::Reject {
        index,
        offset,
        length,
    } = decoded
    {
        assert_eq!(index, 10);
        assert_eq!(offset, 100);
        assert_eq!(length, 16384);
    }
}

#[test]
fn test_a6_03_suggest_roundtrip_index_7() {
    use super::super::serializer::serialize_suggest;
    let msg = BtMessage::Suggest { index: 7 };
    let bytes = serialize_suggest(7);
    let decoded = parse_message(&bytes).unwrap().unwrap();
    assert_eq!(decoded, msg);
    if let BtMessage::Suggest { index } = decoded {
        assert_eq!(index, 7);
    }
}

#[test]
fn test_a6_04_have_all_roundtrip() {
    use super::super::serializer::serialize_have_all;
    let msg = BtMessage::HaveAll;
    let bytes = serialize_have_all();
    let decoded = parse_message(&bytes).unwrap().unwrap();
    assert_eq!(decoded, msg);
    assert!(matches!(decoded, BtMessage::HaveAll));
}

#[test]
fn test_a6_05_have_none_roundtrip() {
    use super::super::serializer::serialize_have_none;
    let msg = BtMessage::HaveNone;
    let bytes = serialize_have_none();
    let decoded = parse_message(&bytes).unwrap().unwrap();
    assert_eq!(decoded, msg);
    assert!(matches!(decoded, BtMessage::HaveNone));
}

#[test]
fn test_a6_06_parse_allowed_fast_dispatch_correct_fields() {
    // BEP 6: AllowedFast has wire ID 17
    let mut data = vec![0, 0, 0, 5, 17];
    data.extend_from_slice(&(99u32).to_be_bytes());
    let msg = parse_message(&data).unwrap().unwrap();
    assert!(matches!(msg, BtMessage::AllowedFast { .. }));
    if let BtMessage::AllowedFast { index } = msg {
        assert_eq!(index, 99);
    }
}

#[test]
fn test_a6_07_parse_reject_dispatch_correct_fields() {
    // BEP 6: Reject has wire ID 16
    let mut data = vec![0, 0, 0, 13, 16];
    data.extend_from_slice(&(5u32).to_be_bytes());
    data.extend_from_slice(&(200u32).to_be_bytes());
    data.extend_from_slice(&(8192u32).to_be_bytes());
    let msg = parse_message(&data).unwrap().unwrap();
    assert!(matches!(msg, BtMessage::Reject { .. }));
    if let BtMessage::Reject {
        index,
        offset,
        length,
    } = msg
    {
        assert_eq!(index, 5);
        assert_eq!(offset, 200);
        assert_eq!(length, 8192);
    }
}

#[test]
fn test_a6_08_allowed_fast_set_tracking_add_and_check() {
    use std::collections::HashSet;
    let mut allowed_fast: HashSet<u32> = HashSet::new();
    assert!(!allowed_fast.contains(&42));
    allowed_fast.insert(42);
    assert!(allowed_fast.contains(&42));
    allowed_fast.insert(7);
    allowed_fast.insert(100);
    assert_eq!(allowed_fast.len(), 3);
    assert!(allowed_fast.contains(&7));
    assert!(allowed_fast.contains(&100));
    assert!(!allowed_fast.contains(&999));
}

// --- BEP 10 Extended message tests ---

#[test]
fn test_parse_extended_handshake() {
    // Extended handshake: ID=20, ext_id=0, bencoded payload
    let bencoded = b"d1:ei0e1:m4:metae"; // dummy bencoded dict
    let mut data = vec![0, 0, 0, 0]; // length placeholder
    let total_len = 1 + 1 + bencoded.len(); // msg_id + ext_id + payload
    data[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    data.push(20); // message ID
    data.push(0); // ext_id = 0 (handshake)
    data.extend_from_slice(bencoded);
    let msg = parse_message(&data).unwrap().unwrap();
    match msg {
        BtMessage::Extended { ext_id, payload } => {
            assert_eq!(ext_id, 0);
            assert_eq!(payload, bencoded.to_vec());
        }
        _ => panic!("Expected Extended message, got {:?}", msg),
    }
}

#[test]
fn test_parse_extended_ut_metadata() {
    // ut_metadata data message: ID=20, ext_id=2 (negotiated)
    let bencoded = b"d8:msg_typei1e5:piecei0ee";
    let mut data = vec![0, 0, 0, 0];
    let total_len = 1 + 1 + bencoded.len();
    data[0..4].copy_from_slice(&(total_len as u32).to_be_bytes());
    data.push(20);
    data.push(2); // ext_id = 2
    data.extend_from_slice(bencoded);
    let msg = parse_message(&data).unwrap().unwrap();
    match msg {
        BtMessage::Extended { ext_id, payload } => {
            assert_eq!(ext_id, 2);
            assert_eq!(payload, bencoded.to_vec());
        }
        _ => panic!("Expected Extended message, got {:?}", msg),
    }
}

#[test]
fn test_extended_roundtrip() {
    use super::super::serializer::serialize_extended;
    let payload = b"d1:ei0ee".to_vec();
    let bytes = serialize_extended(0, payload.clone());
    let decoded = parse_message(&bytes).unwrap().unwrap();
    assert_eq!(decoded, BtMessage::Extended { ext_id: 0, payload });
}

#[test]
fn test_oversized_frame_is_rejected_before_payload_allocation() {
    let length = (super::MAX_BT_MESSAGE_LENGTH + 1) as u32;
    let frame = length.to_be_bytes();
    let error = parse_message(&frame).unwrap_err();
    assert!(
        error.contains("exceeds maximum"),
        "unexpected error: {error}"
    );
}
