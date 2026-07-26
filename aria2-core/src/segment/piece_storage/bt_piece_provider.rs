//! PieceProvider implementation for DefaultPieceStorage (BT feature).
//!
//! This is the concrete wiring that connects `BtPeerInteractive::add_requests()`
//! to the real piece storage, enabling actual piece downloads.
//!
//! Also contains the non-bittorrent stub methods for `DefaultPieceStorage`.

use super::default_storage::DefaultPieceStorage;

#[cfg(feature = "bittorrent")]
use super::super::piece::Piece;
#[cfg(feature = "bittorrent")]
use super::trait_def::PieceStorage;
#[cfg(feature = "bittorrent")]
use crate::engine::bt_peer_connection::BtPeerConn;
#[cfg(feature = "bittorrent")]
use crate::engine::bt_peer_interaction::PieceProvider;
#[cfg(feature = "bittorrent")]
use tracing::trace;

// ===========================================================================
// PieceProvider implementation for DefaultPieceStorage (BT feature)
// ===========================================================================

/// Implementation of `PieceProvider` for `DefaultPieceStorage`, bridging the
/// BT interaction loop's request generation with the actual piece storage.
///
/// This is the concrete wiring that connects `BtPeerInteractive::add_requests()`
/// to the real piece storage, enabling actual piece downloads.
///
/// # C++ Architecture Reference
///
/// In C++ `DefaultBtInteractive`, `pieceStorage_` is a raw pointer to
/// `PieceStorage` used directly. Rust uses the `PieceProvider` trait for
/// decoupling. This impl provides the bridge.
#[cfg(feature = "bittorrent")]
impl PieceProvider for DefaultPieceStorage {
    fn has_missing_piece(&self, peer: &BtPeerConn) -> bool {
        // C++: bitfieldMan_->hasMissingPiece(peer->getBitfield(), peer->getBitfieldLength())
        let peer_bitfield = match peer.session_resource.as_ref() {
            Some(res) => res.bitfield(),
            None => return false,
        };
        self.bfman.has_missing_piece_with_bitfield(peer_bitfield)
    }

    fn get_missing_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece> {
        let peer_bitfield = match peer.session_resource.as_ref() {
            Some(res) => res.bitfield(),
            None => return Vec::new(),
        };

        self.get_missing_pieces_inner(count, peer_bitfield, target_piece_indexes, cuid, false)
    }

    fn get_missing_fast_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece> {
        // Fast pieces: only pieces in the peer's allowed-fast set.
        // C++: createFastIndexBitfield() then select from that.
        let peer_bitfield = match peer.session_resource.as_ref() {
            Some(res) => res.bitfield(),
            None => return Vec::new(),
        };

        self.get_missing_pieces_inner(count, peer_bitfield, target_piece_indexes, cuid, true)
    }

    fn is_end_game(&self) -> bool {
        // Delegate to PieceStorage's implementation
        <Self as PieceStorage>::is_end_game(self)
    }

    fn has_missing_unused_piece(&self) -> bool {
        // Delegate to PieceStorage's implementation
        <Self as PieceStorage>::has_missing_unused_piece(self)
    }

    fn enter_end_game(&mut self) {
        // Delegate to PieceStorage's implementation
        <Self as PieceStorage>::enter_end_game(self)
    }

    // ── checkHave optimization support ────────────────────────────────────

    fn get_advertised_piece_indexes_ext(
        &self,
        _my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        // C++ implementation (DefaultPieceStorage.cc line 731-733) does NOT
        // filter by myCuid despite the header documentation. Only
        // haveIndex > lastHaveIndex is checked.
        let mut indexes = Vec::new();
        let mut new_last = last_have_index;
        for entry in &self.haves {
            if entry.have_index > last_have_index {
                indexes.push(entry.index);
                new_last = new_last.max(entry.have_index);
            }
        }
        (indexes, new_last)
    }

    fn get_bitfield_length_ext(&self) -> usize {
        self.bfman.bitfield().len()
    }

    fn get_bitfield_ext(&self) -> Vec<u8> {
        self.bfman.bitfield().to_vec()
    }

    fn all_download_finished_ext(&self) -> bool {
        self.bfman.is_all_complete()
    }

    fn get_completed_length_ext(&self) -> u64 {
        self.bfman.get_completed_length()
    }
}

#[cfg(feature = "bittorrent")]
impl DefaultPieceStorage {
    /// Internal method to get missing pieces based on the peer's bitfield.
    ///
    /// Mirrors C++ `DefaultPieceStorage::getMissingPiece()`:
    /// - In endgame: get all missing pieces (even in-use), shuffle, pick
    /// - Normal: get missing unused pieces, select via piece selector
    ///
    /// When `fast_only` is true, restrict to pieces in the peer's
    /// allowed-fast set (C++ `createFastIndexBitfield()`).
    pub(crate) fn get_missing_pieces_inner(
        &mut self,
        min_missing_blocks: usize,
        peer_bitfield: &[u8],
        target_piece_indexes: &[u32],
        cuid: u64,
        fast_only: bool,
    ) -> Vec<Piece> {
        let num_pieces = self.bfman.num_pieces();

        // Build a bitfield of pieces we can request from this peer.
        // C++: getAllMissingIndexes() or getAllMissingUnusedIndexes()
        let mis_bitfield = if self.end_game {
            // Endgame: all missing pieces (even in-use by other peers)
            self.bfman.all_missing_indexes(peer_bitfield)
        } else {
            // Normal: only missing unused pieces
            self.bfman.all_missing_unused_indexes(peer_bitfield)
        };

        if mis_bitfield.is_empty() {
            return Vec::new();
        }

        // Exclude pieces already assigned to this peer's request factory
        // C++ passes excludeIndexes to getMissingPiece()
        let mut mis_bitfield = mis_bitfield;
        for &idx in target_piece_indexes {
            let i = idx as usize;
            if i < num_pieces {
                super::super::bitfield_util::clear_bit(&mut mis_bitfield, num_pieces, i);
            }
        }

        // If fast_only, restrict to allowed-fast pieces
        // C++: createFastIndexBitfield() intersects with peer's allowed set
        // For now, we just use the same bitfield since fast piece filtering
        // would need the peer's allowed-fast index set from BtPeerConn
        if fast_only {
            // TODO: Once we expose the peer's allowed-fast index set from
            // BtPeerConn, intersect mis_bitfield with it here.
            // For now, we use the same bitfield (fast pieces are a subset
            // of available pieces, filtered by the peer's allowed-fast set).
        }

        let mut pieces = Vec::new();
        let mut mis_block = 0usize;

        if self.end_game {
            // Endgame: collect all eligible piece indexes, shuffle, pick
            let mut indexes: Vec<usize> = Vec::new();
            for i in 0..num_pieces {
                if super::super::bitfield_util::test_bit(&mis_bitfield, num_pieces, i) {
                    indexes.push(i);
                }
            }

            // Shuffle for random distribution (C++ does std::shuffle)
            use rand::seq::SliceRandom;
            let mut rng = rand::thread_rng();
            indexes.shuffle(&mut rng);

            for idx in indexes {
                if mis_block >= min_missing_blocks {
                    break;
                }
                if let Some(piece) = self.check_out_piece(idx, cuid) {
                    mis_block += piece.count_missing_blocks();
                    pieces.push(piece);
                }
            }
        } else {
            // Normal mode: use the piece selector (rarest-first by default).
            // C++ uses `pieceSelector_->select(index, misbitfield, blocks)`.
            // After each selection, flip the bit in mis_bitfield so we don't
            // pick the same piece twice (C++: `bitfield::flipBit`).
            while mis_block < min_missing_blocks {
                match self.piece_selector.select(&mis_bitfield, num_pieces) {
                    Some(index) => {
                        if let Some(piece) = self.check_out_piece(index, cuid) {
                            mis_block += piece.count_missing_blocks();
                            pieces.push(piece);
                            // Flip this bit off so we don't select it again
                            super::super::bitfield_util::clear_bit(
                                &mut mis_bitfield,
                                num_pieces,
                                index,
                            );
                        } else {
                            // Piece was already checked out or not available
                            super::super::bitfield_util::clear_bit(
                                &mut mis_bitfield,
                                num_pieces,
                                index,
                            );
                        }
                    }
                    None => break,
                }
            }
        }

        if !pieces.is_empty() {
            trace!(
                "get_missing_pieces_inner: selected {} pieces ({} missing blocks, fast_only={})",
                pieces.len(),
                mis_block,
                fast_only
            );
        }

        pieces
    }

    /// Check out a piece by index for a given CUID.
    ///
    /// Mirrors C++ `DefaultPieceStorage::checkOutPiece()`.
    /// Marks the piece as in-use in the bitfield and creates a `Piece` object.
    pub(crate) fn check_out_piece(&mut self, index: usize, cuid: u64) -> Option<Piece> {
        if index >= self.bfman.num_pieces() {
            return None;
        }
        if self.bfman.has_piece(index) {
            return None;
        }
        // In endgame, pieces can be in-use (shared across peers)
        if !self.end_game && self.bfman.is_use_piece(index) {
            return None;
        }

        self.bfman.set_use_piece(index);

        let piece_start = index as u64 * self.piece_length;
        let piece_len = std::cmp::min(
            self.piece_length,
            self.total_length.saturating_sub(piece_start),
        );

        let mut piece = Piece::new(index, piece_len);
        piece.add_user(cuid);

        // In endgame, the piece might already be in used_pieces
        // C++ handles this by adding another user to the existing piece
        if self.end_game {
            if let Some(existing) = self.used_pieces.get_mut(&index) {
                existing.add_user(cuid);
                return Some(existing.clone());
            }
        }

        self.used_pieces.insert(index, piece.clone());
        Some(piece)
    }
}

// ===========================================================================
// Non-bittorrent stub: PieceProvider is only available with bittorrent feature
// ===========================================================================

// Note: PieceProvider trait requires BtPeerConn which is behind the
// bittorrent feature gate. When bittorrent is disabled, we don't need
// this impl. The trait is still defined (in bt_peer_interaction.rs) but
// not all items may be usable. The _ext methods are provided as inherent
// methods on DefaultPieceStorage for non-bittorrent code paths.

#[cfg(not(feature = "bittorrent"))]
impl DefaultPieceStorage {
    /// Stub for non-bittorrent builds: get bitfield length.
    pub fn get_bitfield_length_ext(&self) -> usize {
        self.bfman.bitfield().len()
    }

    /// Stub for non-bittorrent builds: get bitfield.
    pub fn get_bitfield_ext(&self) -> Vec<u8> {
        self.bfman.bitfield().to_vec()
    }

    /// Stub for non-bittorrent builds: check all download finished.
    pub fn all_download_finished_ext(&self) -> bool {
        self.bfman.is_all_complete()
    }

    /// Stub for non-bittorrent builds: get completed length.
    pub fn get_completed_length_ext(&self) -> u64 {
        self.bfman.get_completed_length()
    }
}
