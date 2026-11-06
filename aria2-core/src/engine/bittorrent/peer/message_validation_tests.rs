use super::*;

/// Helper: create a validator for a 1000-piece torrent with 256 KiB pieces.
fn validator_1k() -> BtMessageValidator {
    BtMessageValidator::new(1000, 262144)
}

// -- validate_index ----------------------------------------------------

#[test]
fn valid_piece_index() {
    let v = validator_1k();
    assert!(v.validate_index(0).is_ok());
    assert!(v.validate_index(999).is_ok());
}

#[test]
fn invalid_piece_index_equal_to_num_pieces() {
    let v = validator_1k();
    let err = v.validate_index(1000).unwrap_err();
    assert_eq!(
        err,
        BtMessageValidationError::IndexOutOfRange {
            index: 1000,
            num_pieces: 1000,
        }
    );
}

#[test]
fn invalid_piece_index_way_over() {
    let v = validator_1k();
    let err = v.validate_index(u32::MAX).unwrap_err();
    assert_eq!(
        err,
        BtMessageValidationError::IndexOutOfRange {
            index: u32::MAX,
            num_pieces: 1000,
        }
    );
}

// -- validate_range ----------------------------------------------------

#[test]
fn valid_block_range() {
    let v = validator_1k();
    assert!(v.validate_range(0, 0, 16384).is_ok());
    assert!(v.validate_range(500, 262144 - 16384, 16384).is_ok());
}

#[test]
fn invalid_block_range_begin_plus_length_exceeds_piece_length() {
    let v = validator_1k();
    let err = v.validate_range(0, 262140, 16).unwrap_err();
    assert_eq!(
        err,
        BtMessageValidationError::BlockOutOfRange {
            index: 0,
            begin: 262140,
            length: 16,
            piece_length: 262144,
        }
    );
}

#[test]
fn invalid_block_range_overflow() {
    let v = validator_1k();
    // begin + length overflows u32
    let err = v.validate_range(0, u32::MAX - 1, 3).unwrap_err();
    assert!(matches!(
        err,
        BtMessageValidationError::BlockOutOfRange { .. }
    ));
}

#[test]
fn invalid_block_length_exceeds_max() {
    let v = validator_1k();
    let err = v.validate_range(0, 0, 65537).unwrap_err();
    assert_eq!(
        err,
        BtMessageValidationError::InvalidBlockLength {
            length: 65537,
            max_block_length: MAX_BLOCK_LENGTH,
        }
    );
}

// -- validate_piece ----------------------------------------------------

#[test]
fn valid_piece_message() {
    let v = validator_1k();
    assert!(v.validate_piece(0, 0, 16384).is_ok());
}

#[test]
fn invalid_piece_message_empty_data() {
    let v = validator_1k();
    let err = v.validate_piece(0, 0, 0).unwrap_err();
    assert_eq!(
        err,
        BtMessageValidationError::BlockOutOfRange {
            index: 0,
            begin: 0,
            length: 0,
            piece_length: 262144,
        }
    );
}

#[test]
fn invalid_piece_message_bad_index() {
    let v = validator_1k();
    let err = v.validate_piece(2000, 0, 1024).unwrap_err();
    assert!(matches!(
        err,
        BtMessageValidationError::IndexOutOfRange { .. }
    ));
}

#[test]
fn invalid_piece_message_block_out_of_range() {
    let v = validator_1k();
    let err = v.validate_piece(0, 262140, 16).unwrap_err();
    assert!(matches!(
        err,
        BtMessageValidationError::BlockOutOfRange { .. }
    ));
}

// -- validate_bitfield -------------------------------------------------

#[test]
fn valid_bitfield() {
    // 1000 pieces -> ceil(1000/8) = 125 bytes
    let v = validator_1k();
    assert!(v.validate_bitfield(&[0u8; 125]).is_ok());
}

#[test]
fn invalid_bitfield_too_short() {
    let v = validator_1k();
    let err = v.validate_bitfield(&[0u8; 124]).unwrap_err();
    assert_eq!(
        err,
        BtMessageValidationError::BitfieldLengthMismatch {
            bitfield_len: 124,
            expected_pieces: 1000,
        }
    );
}

#[test]
fn invalid_bitfield_too_long() {
    let v = validator_1k();
    let err = v.validate_bitfield(&[0u8; 126]).unwrap_err();
    assert!(matches!(
        err,
        BtMessageValidationError::BitfieldLengthMismatch { .. }
    ));
}

#[test]
fn bitfield_trailing_bits_are_msb_first() {
    let v = BtMessageValidator::new(10, 262144);

    // BitTorrent numbers bits from the most significant bit. For ten
    // pieces, the first two bits of the final byte are meaningful.
    assert!(v.validate_bitfield(&[0xFF, 0b1100_0000]).is_ok());
    assert!(v.validate_bitfield(&[0xFF, 0b0000_0011]).is_err());
}

// -- validate_handshake ------------------------------------------------

#[test]
fn handshake_info_hash_match() {
    let hash = [0xAA; 20];
    let v = BtMessageValidator::new(100, 262144).with_expected_info_hash(hash);
    assert!(v.validate_handshake(&hash).is_ok());
}

#[test]
fn handshake_info_hash_mismatch() {
    let expected = [0xAA; 20];
    let received = [0xBB; 20];
    let v = BtMessageValidator::new(100, 262144).with_expected_info_hash(expected);
    let err = v.validate_handshake(&received).unwrap_err();
    assert_eq!(err, BtMessageValidationError::InfoHashMismatch);
}

#[test]
fn handshake_no_expected_hash_always_passes() {
    let v = BtMessageValidator::new(100, 262144);
    assert!(v.validate_handshake(&[0xFF; 20]).is_ok());
}

// -- metadata_get_mode -------------------------------------------------

#[test]
fn metadata_get_mode_skips_index_validation() {
    let v = BtMessageValidator::new(10, 262144).with_metadata_get_mode(true);
    // index 9999 would normally fail, but metadata mode skips it
    assert!(v.validate_index(9999).is_ok());
}

#[test]
fn metadata_get_mode_skips_bitfield_validation() {
    let v = BtMessageValidator::new(10, 262144).with_metadata_get_mode(true);
    // wrong length but skipped
    assert!(v.validate_bitfield(&[0u8; 999]).is_ok());
}

#[test]
fn metadata_get_mode_skips_range_validation() {
    let v = BtMessageValidator::new(10, 262144).with_metadata_get_mode(true);
    assert!(v.validate_range(9999, 0, 999999).is_ok());
}

#[test]
fn metadata_get_mode_does_not_skip_handshake() {
    // Handshake validation is still enforced in metadata mode
    let expected = [0xAA; 20];
    let received = [0xBB; 20];
    let v = BtMessageValidator::new(10, 262144)
        .with_expected_info_hash(expected)
        .with_metadata_get_mode(true);
    assert!(v.validate_handshake(&received).is_err());
}

// -- validate dispatch -------------------------------------------------

#[test]
fn validate_dispatches_have() {
    let v = validator_1k();
    let msg = BtMessage::Have { piece_index: 500 };
    assert!(v.validate(&msg).is_ok());
    let bad = BtMessage::Have { piece_index: 2000 };
    assert!(v.validate(&bad).is_err());
}

#[test]
fn validate_dispatches_bitfield() {
    let v = validator_1k();
    let msg = BtMessage::Bitfield {
        data: vec![0u8; 125],
    };
    assert!(v.validate(&msg).is_ok());
    let bad = BtMessage::Bitfield {
        data: vec![0u8; 10],
    };
    assert!(v.validate(&bad).is_err());
}

#[test]
fn validate_dispatches_request() {
    let v = validator_1k();
    let msg = BtMessage::Request {
        request: aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(0, 0, 16384),
    };
    assert!(v.validate(&msg).is_ok());
}

#[test]
fn validate_dispatches_cancel() {
    let v = validator_1k();
    let msg = BtMessage::Cancel {
        request: aria2_protocol::bittorrent::message::types::PieceBlockRequest::new(0, 0, 16384),
    };
    assert!(v.validate(&msg).is_ok());
}

#[test]
fn validate_dispatches_reject() {
    let v = validator_1k();
    let msg = BtMessage::Reject {
        index: 0,
        offset: 0,
        length: 16384,
    };
    assert!(v.validate(&msg).is_ok());
}

#[test]
fn validate_dispatches_piece() {
    let v = validator_1k();
    let msg = BtMessage::Piece {
        index: 0,
        begin: 0,
        data: vec![0u8; 16384].into(),
    };
    assert!(v.validate(&msg).is_ok());
}

#[test]
fn validate_no_constraint_messages_pass() {
    let v = validator_1k();
    for msg in [
        BtMessage::KeepAlive,
        BtMessage::Choke,
        BtMessage::Unchoke,
        BtMessage::Interested,
        BtMessage::NotInterested,
        BtMessage::HaveAll,
        BtMessage::HaveNone,
        BtMessage::Port { port: 6881 },
        BtMessage::Extended {
            ext_id: 0,
            payload: vec![],
        },
    ] {
        assert!(v.validate(&msg).is_ok(), "failed for {:?}", msg);
    }
}

// -- edge cases --------------------------------------------------------

#[test]
fn zero_num_pieces_rejects_all_indices() {
    let v = BtMessageValidator::new(0, 262144);
    assert!(v.validate_index(0).is_err());
}

#[test]
fn bitfield_for_one_piece() {
    let v = BtMessageValidator::new(1, 262144);
    assert!(v.validate_bitfield(&[0u8; 1]).is_ok());
    assert!(v.validate_bitfield(&[0u8; 0]).is_err());
}

#[test]
fn display_error_messages() {
    let err = BtMessageValidationError::IndexOutOfRange {
        index: 5,
        num_pieces: 3,
    };
    assert!(err.to_string().contains("5"));
    assert!(err.to_string().contains("3"));

    let err = BtMessageValidationError::InfoHashMismatch;
    assert!(err.to_string().contains("mismatch"));
}
