//! Semantic message validation framework for BitTorrent peer messages.
//!
//! While `parse_message()` validates structural integrity (correct length, valid
//! message type), this module provides domain-level validation that checks
//! constraints such as piece index bounds, block range validity, bitfield length
//! consistency, and info-hash matching — mirroring the per-message validators
//! in the C++ aria2 implementation.

use std::fmt;

use crate::bittorrent::message::types::BtMessage;

/// Maximum block length (64 KiB), matching C++ aria2 `BtConstants.h` `MAX_BLOCK_LENGTH`.
///
/// Note: While aria2 typically *requests* 16 KiB blocks (`BLOCK_SIZE`), the
/// protocol allows peers to send blocks up to 64 KiB. Using 16 KiB here would
/// incorrectly reject valid Piece/Request messages with larger blocks.
pub const MAX_BLOCK_LENGTH: u32 = 64 * 1024;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors produced by [`BtMessageValidator`] when a message fails domain
/// validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BtMessageValidationError {
    /// Piece index exceeds the total number of pieces in the torrent.
    IndexOutOfRange { index: u32, num_pieces: u32 },
    /// `begin + length` exceeds the piece length for the given piece index.
    BlockOutOfRange {
        index: u32,
        begin: u32,
        length: u32,
        piece_length: u32,
    },
    /// Bitfield byte count does not match the expected number of pieces.
    BitfieldLengthMismatch {
        bitfield_len: usize,
        expected_pieces: u32,
    },
    /// Received info-hash does not match the expected hash.
    InfoHashMismatch,
    /// Block length exceeds the configured maximum.
    InvalidBlockLength { length: u32, max_block_length: u32 },
}

impl fmt::Display for BtMessageValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IndexOutOfRange { index, num_pieces } => {
                write!(
                    f,
                    "piece index {} out of range (num_pieces={})",
                    index, num_pieces
                )
            }
            Self::BlockOutOfRange {
                index,
                begin,
                length,
                piece_length,
            } => {
                write!(
                    f,
                    "block out of range: index={}, begin={}, length={}, piece_length={}",
                    index, begin, length, piece_length
                )
            }
            Self::BitfieldLengthMismatch {
                bitfield_len,
                expected_pieces,
            } => {
                let expected_bytes = (*expected_pieces as usize).div_ceil(8);
                write!(
                    f,
                    "bitfield length {} does not match expected {} bytes ({} pieces)",
                    bitfield_len, expected_bytes, expected_pieces
                )
            }
            Self::InfoHashMismatch => write!(f, "info-hash mismatch"),
            Self::InvalidBlockLength {
                length,
                max_block_length,
            } => {
                write!(
                    f,
                    "block length {} exceeds maximum {}",
                    length, max_block_length
                )
            }
        }
    }
}

impl std::error::Error for BtMessageValidationError {}

// ---------------------------------------------------------------------------
// Validator
// ---------------------------------------------------------------------------

/// Domain-level validator for BitTorrent peer messages.
///
/// Construct with the torrent metadata (`num_pieces`, `piece_length`) and
/// optionally configure an expected info-hash or metadata-get mode.
///
/// # Example
///
/// ```
/// use aria2_protocol::bittorrent::message::validation::BtMessageValidator;
///
/// let validator = BtMessageValidator::new(1000, 262144)
///     .with_expected_info_hash([0u8; 20]);
///
/// assert!(validator.validate_index(500).is_ok());
/// assert!(validator.validate_index(2000).is_err());
/// ```
#[derive(Debug, Clone)]
pub struct BtMessageValidator {
    /// Total number of pieces in the torrent.
    pub num_pieces: u32,
    /// Byte length of each piece (the last piece may be shorter).
    pub piece_length: u32,
    /// Maximum allowed block length (default 16 KiB).
    pub max_block_length: u32,
    /// Expected info-hash for handshake validation.
    pub expected_info_hash: Option<[u8; 20]>,
    /// When true, skip piece/bitfield validation (metadata-only downloads
    /// where num_pieces/piece_length may not be meaningful).
    pub metadata_get_mode: bool,
}

impl BtMessageValidator {
    /// Create a new validator with the given torrent parameters.
    ///
    /// `max_block_length` defaults to [`MAX_BLOCK_LENGTH`] (16 KiB).
    /// `expected_info_hash` defaults to `None`.
    /// `metadata_get_mode` defaults to `false`.
    pub fn new(num_pieces: u32, piece_length: u32) -> Self {
        Self {
            num_pieces,
            piece_length,
            max_block_length: MAX_BLOCK_LENGTH,
            expected_info_hash: None,
            metadata_get_mode: false,
        }
    }

    /// Set the expected info-hash for handshake validation.
    pub fn with_expected_info_hash(mut self, hash: [u8; 20]) -> Self {
        self.expected_info_hash = Some(hash);
        self
    }

    /// Enable or disable metadata-get mode.
    ///
    /// When enabled, piece index, block range, and bitfield validations are
    /// skipped because `num_pieces` / `piece_length` may not reflect the
    /// actual torrent data (the peer is only fetching metadata).
    pub fn with_metadata_get_mode(mut self, mode: bool) -> Self {
        self.metadata_get_mode = mode;
        self
    }

    // -----------------------------------------------------------------------
    // Top-level dispatch
    // -----------------------------------------------------------------------

    /// Validate a parsed [`BtMessage`] against domain constraints.
    ///
    /// Messages that carry no domain payload (KeepAlive, Choke, Unchoke,
    /// Interested, NotInterested, HaveAll, HaveNone, Port, Extended) always
    /// pass validation.
    pub fn validate(&self, msg: &BtMessage) -> Result<(), BtMessageValidationError> {
        match msg {
            BtMessage::Have { piece_index } => self.validate_index(*piece_index),
            BtMessage::Bitfield { data } => self.validate_bitfield(data),
            BtMessage::Request { request } => {
                self.validate_range(request.index, request.begin, request.length)
            }
            BtMessage::Piece { index, begin, data } => {
                self.validate_piece(*index, *begin, data.len())
            }
            BtMessage::Cancel { request } => {
                self.validate_range(request.index, request.begin, request.length)
            }
            BtMessage::Reject {
                index,
                offset,
                length,
            } => self.validate_range(*index, *offset, *length),
            BtMessage::AllowedFast { index } => self.validate_index(*index),
            BtMessage::Suggest { index } => self.validate_index(*index),
            // No domain constraints to validate for these variants.
            BtMessage::KeepAlive
            | BtMessage::Choke
            | BtMessage::Unchoke
            | BtMessage::Interested
            | BtMessage::NotInterested
            | BtMessage::HaveAll
            | BtMessage::HaveNone
            | BtMessage::Port { .. }
            | BtMessage::Extended { .. } => Ok(()),
        }
    }

    // -----------------------------------------------------------------------
    // Per-constraint validation
    // -----------------------------------------------------------------------

    /// Validate a piece index (used for Have, SuggestPiece, AllowedFast).
    pub fn validate_index(&self, index: u32) -> Result<(), BtMessageValidationError> {
        if self.metadata_get_mode {
            return Ok(());
        }
        if index >= self.num_pieces {
            return Err(BtMessageValidationError::IndexOutOfRange {
                index,
                num_pieces: self.num_pieces,
            });
        }
        Ok(())
    }

    /// Validate a block range (used for Request, Cancel, Reject).
    ///
    /// Checks:
    /// 1. `index < num_pieces`
    /// 2. `length != 0` (C++ `checkLength` rejects zero-length blocks)
    /// 3. `begin + length <= piece_length` (with overflow protection)
    /// 4. `length <= max_block_length`
    pub fn validate_range(
        &self,
        index: u32,
        begin: u32,
        length: u32,
    ) -> Result<(), BtMessageValidationError> {
        if self.metadata_get_mode {
            return Ok(());
        }
        self.validate_index(index)?;
        // C++ checkLength rejects length == 0 for Request/Cancel/Reject.
        if length == 0 {
            return Err(BtMessageValidationError::BlockOutOfRange {
                index,
                begin,
                length: 0,
                piece_length: self.piece_length,
            });
        }
        // Overflow-safe addition: if begin + length overflows u32 it is
        // certainly larger than piece_length.
        let end = begin.checked_add(length);
        match end {
            Some(e) if e <= self.piece_length => {}
            _ => {
                return Err(BtMessageValidationError::BlockOutOfRange {
                    index,
                    begin,
                    length,
                    piece_length: self.piece_length,
                });
            }
        }
        if length > self.max_block_length {
            return Err(BtMessageValidationError::InvalidBlockLength {
                length,
                max_block_length: self.max_block_length,
            });
        }
        Ok(())
    }

    /// Validate a Piece message's data block.
    ///
    /// The same checks as [`validate_range`] but using the actual data length
    /// (which may differ from the requested length for the final block of a
    /// piece).
    pub fn validate_piece(
        &self,
        index: u32,
        begin: u32,
        data_len: usize,
    ) -> Result<(), BtMessageValidationError> {
        if self.metadata_get_mode {
            return Ok(());
        }
        self.validate_index(index)?;
        let length = data_len as u32;
        let end = begin.checked_add(length);
        match end {
            Some(e) if e <= self.piece_length => {}
            _ => {
                return Err(BtMessageValidationError::BlockOutOfRange {
                    index,
                    begin,
                    length,
                    piece_length: self.piece_length,
                });
            }
        }
        // Piece messages are allowed to carry up to max_block_length bytes.
        // The final block of a piece may be shorter, so we only reject
        // blocks that exceed the maximum.
        if length > self.max_block_length {
            return Err(BtMessageValidationError::InvalidBlockLength {
                length,
                max_block_length: self.max_block_length,
            });
        }
        Ok(())
    }

    /// Validate a Bitfield message's payload length.
    ///
    /// The bitfield must be exactly `ceil(num_pieces / 8)` bytes long.
    /// Additionally, unused bits in the last byte must be zero (C++ `checkBitfield`).
    pub fn validate_bitfield(&self, data: &[u8]) -> Result<(), BtMessageValidationError> {
        if self.metadata_get_mode {
            return Ok(());
        }
        let expected_bytes = (self.num_pieces as usize).div_ceil(8);
        if data.len() != expected_bytes {
            return Err(BtMessageValidationError::BitfieldLengthMismatch {
                bitfield_len: data.len(),
                expected_pieces: self.num_pieces,
            });
        }
        // C++ checkBitfield: verify that unused bits in the last byte are zero.
        // If num_pieces is not a multiple of 8, the last byte has some
        // unused high bits that must be zero.
        let remainder = (self.num_pieces as usize) % 8;
        if remainder != 0
            && let Some(&last_byte) = data.last()
        {
            // The valid bits in the last byte are the low `remainder` bits.
            // All higher bits must be zero.
            let mask: u8 = !((1 << remainder) - 1);
            if last_byte & mask != 0 {
                return Err(BtMessageValidationError::BitfieldLengthMismatch {
                    bitfield_len: data.len(),
                    expected_pieces: self.num_pieces,
                });
            }
        }
        Ok(())
    }

    /// Validate a Handshake message's info-hash.
    ///
    /// If no expected info-hash has been configured, validation always passes.
    pub fn validate_handshake(
        &self,
        received_hash: &[u8; 20],
    ) -> Result<(), BtMessageValidationError> {
        if let Some(expected) = self.expected_info_hash
            && received_hash != &expected
        {
            return Err(BtMessageValidationError::InfoHashMismatch);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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
            request: crate::bittorrent::message::types::PieceBlockRequest::new(0, 0, 16384),
        };
        assert!(v.validate(&msg).is_ok());
    }

    #[test]
    fn validate_dispatches_cancel() {
        let v = validator_1k();
        let msg = BtMessage::Cancel {
            request: crate::bittorrent::message::types::PieceBlockRequest::new(0, 0, 16384),
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
}
