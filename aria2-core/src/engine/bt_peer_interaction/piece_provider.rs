//! PieceProvider trait — abstraction for PieceStorage dependency
//!
//! In C++ `DefaultBtInteractive`, `pieceStorage_` is a raw pointer used
//! for `hasMissingPiece()`, `getMissingPiece()`, `isEndGame()`,
//! `hasMissingUnusedPiece()`, and `enterEndGame()`. This trait exposes
//! those operations so the interaction loop remains decoupled from
//! the full `PieceStorage` trait.

use crate::engine::bt_peer_connection::BtPeerConn;
use crate::segment::piece::Piece;

/// Trait abstracting the piece storage operations needed by the BT
/// interaction loop for request generation.
///
/// Note: Some methods (`is_end_game`, `has_missing_unused_piece`,
/// `enter_end_game`) also exist on `PieceStorage`. For types that
/// implement both traits, call via unambiguous syntax:
/// `PieceProvider::is_end_game(&storage)` or `PieceStorage::is_end_game(&storage)`.
pub trait PieceProvider: Send + Sync {
    /// Check if the peer has pieces we still need.
    /// Mirrors C++ `PieceStorage::hasMissingPiece(peer)`.
    fn has_missing_piece(&self, peer: &BtPeerConn) -> bool;

    /// Get missing pieces for this peer, up to `count` pieces.
    /// Mirrors C++ `PieceStorage::getMissingPiece(pieces, count, peer, cuid)`.
    ///
    /// In the C++ code, `getMissingPiece` fills the `pieces` vector with
    /// up to `count` pieces. The Rust version returns a `Vec<Piece>`.
    ///
    /// The `target_piece_indexes` parameter lists pieces already assigned
    /// to this peer (from `BtRequestFactory::getTargetPieceIndexes()`),
    /// so the storage can avoid assigning the same piece twice.
    fn get_missing_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece>;

    /// Get missing fast-extension pieces for a choked peer.
    /// Mirrors C++ `PieceStorage::getMissingFastPiece(pieces, count, peer, indexes, cuid)`.
    fn get_missing_fast_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece>;

    /// Check whether end-game mode is active.
    /// Mirrors C++ `PieceStorage::isEndGame()`.
    fn is_end_game(&self) -> bool;

    /// Check if there are missing pieces that are not in-use by any peer.
    /// Mirrors C++ `PieceStorage::hasMissingUnusedPiece()`.
    fn has_missing_unused_piece(&self) -> bool;

    /// Enter end-game mode.
    /// Mirrors C++ `PieceStorage::enterEndGame()`.
    fn enter_end_game(&mut self);

    // ── checkHave optimization support ──────────────────────────────────

    /// Get piece indexes advertised since `last_have_index` by CUIDs other
    /// than `my_cuid`. Returns (indexes, new_last_have_index).
    /// Mirrors C++ `PieceStorage::getAdvertisedPieceIndexes()`.
    fn get_advertised_piece_indexes_ext(
        &self,
        my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64);

    /// Returns the bitfield byte length.
    /// Mirrors C++ `PieceStorage::getBitfieldLength()`.
    fn get_bitfield_length_ext(&self) -> usize;

    /// Returns the completion bitfield.
    /// Mirrors C++ `PieceStorage::getBitfield()`.
    fn get_bitfield_ext(&self) -> Vec<u8>;

    /// Check if all downloads are finished (ignoring filter).
    /// Mirrors C++ `PieceStorage::allDownloadFinished()`.
    fn all_download_finished_ext(&self) -> bool;

    /// Returns the total completed length in bytes.
    /// Mirrors C++ `PieceStorage::getCompletedLength()`.
    fn get_completed_length_ext(&self) -> u64;

    /// Create a serialized Bitfield message from the current piece completion state.
    ///
    /// Mirrors C++ `DefaultBtMessageFactory::createBitfieldMessage()` which
    /// reads the bitfield from `PieceStorage` and wraps it in a BT Bitfield
    /// message (ID=5).
    ///
    /// Returns `None` if the bitfield is empty (no pieces or zero-length).
    fn create_bitfield_message(&self) -> Option<Vec<u8>> {
        let bf = self.get_bitfield_ext();
        if bf.is_empty() {
            return None;
        }
        use aria2_protocol::bittorrent::message::serializer::serialize_bitfield;
        Some(serialize_bitfield(bf))
    }
}
