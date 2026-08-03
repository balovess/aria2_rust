//! Pipelining and request queue management — target piece lifecycle.

use tracing::debug;

use super::BtRequestFactory;
use crate::segment::piece::Piece;

impl BtRequestFactory {
    /// Add a target piece to this peer's responsibility.
    ///
    /// Mirrors C++ `addTargetPiece()`.
    pub fn add_target_piece(&mut self, piece: Piece) {
        debug!(
            "BtRequestFactory: added target piece index={}",
            piece.index()
        );
        self.pieces.push_back(piece);
    }

    /// Remove a specific target piece by index.
    ///
    /// Mirrors C++ `removeTargetPiece()`. Cancels the piece in piece storage
    /// and returns the removed piece (if found).
    ///
    /// The caller should call `BtMessageDispatcher::do_abort_outstanding_request_action()`
    /// for the returned piece index.
    pub fn remove_target_piece(&mut self, piece_index: u32) -> Option<Piece> {
        let pos = self
            .pieces
            .iter()
            .position(|p| p.index() == piece_index as usize)?;
        let piece = self.pieces.remove(pos)?;

        debug!(
            "BtRequestFactory: removed target piece index={}",
            piece.index()
        );

        // Cancel in piece storage (C++ does pieceStorage_->cancelPiece(piece, cuid_))
        if let Some(ref storage) = self.piece_storage {
            storage.cancel_piece(piece.index(), self.cuid);
        }

        Some(piece)
    }

    /// Remove all target pieces.
    ///
    /// Mirrors C++ `removeAllTargetPiece()`. Cancels all pieces in piece storage.
    ///
    /// Returns the list of removed pieces so the caller can abort outstanding
    /// requests for each one.
    pub fn remove_all_target_pieces(&mut self) -> Vec<Piece> {
        let removed: Vec<Piece> = self.pieces.drain(..).collect();

        for piece in &removed {
            debug!(
                "BtRequestFactory: removing target piece index={}",
                piece.index()
            );
            // C++ does dispatcher_->doAbortOutstandingRequestAction(elem)
            // and pieceStorage_->cancelPiece(elem, cuid_)
            if let Some(ref storage) = self.piece_storage {
                storage.cancel_piece(piece.index(), self.cuid);
            }
        }

        removed
    }

    /// Return the number of target pieces.
    ///
    /// Mirrors C++ `countTargetPiece()`.
    pub fn count_target_piece(&self) -> usize {
        self.pieces.len()
    }

    /// Return the total number of missing blocks across all target pieces.
    ///
    /// Mirrors C++ `countMissingBlock()`.
    pub fn count_missing_block(&self) -> usize {
        self.pieces.iter().map(|p| p.count_missing_blocks()).sum()
    }

    /// Remove completed pieces from the target list.
    ///
    /// Mirrors C++ `removeCompletedPiece()`. Before removing, the C++ version
    /// calls `dispatcher_->doAbortOutstandingRequestAction(piece)` for each
    /// completed piece. Here we return the removed piece indices so the caller
    /// can perform the abort.
    ///
    /// Returns the indices of removed pieces.
    pub fn remove_completed_piece(&mut self) -> Vec<u32> {
        let mut removed_indices = Vec::new();
        let mut i = 0;
        while i < self.pieces.len() {
            if self.pieces[i].is_complete() {
                let piece = self.pieces.remove(i).unwrap();
                debug!(
                    "BtRequestFactory: removed completed piece index={}",
                    piece.index()
                );
                removed_indices.push(piece.index() as u32);
            } else {
                i += 1;
            }
        }
        removed_indices
    }

    /// Handle choked action — remove target pieces not in the allowed-fast set.
    ///
    /// Mirrors C++ `doChokedAction()`. Pieces whose index is NOT in the
    /// peer's allowed-fast set are removed and cancelled in piece storage.
    ///
    /// The `is_in_allowed_fast` closure should return `true` if the given
    /// piece index is in the peer's allowed-fast set (mirroring
    /// `Peer::isInPeerAllowedIndexSet()`).
    ///
    /// Returns the indices of removed pieces so the caller can abort
    /// outstanding requests for them.
    pub fn do_choked_action(&mut self, is_in_allowed_fast: impl Fn(u32) -> bool) -> Vec<u32> {
        let mut removed_indices = Vec::new();

        // First pass: cancel in piece storage for pieces not in allowed-fast
        for piece in &self.pieces {
            if !is_in_allowed_fast(piece.index() as u32) {
                debug!(
                    "BtRequestFactory: choked action cancelling piece index={}",
                    piece.index()
                );
                if let Some(ref storage) = self.piece_storage {
                    storage.cancel_piece(piece.index(), self.cuid);
                }
            }
        }

        // Second pass: remove from the deque
        let mut i = 0;
        while i < self.pieces.len() {
            if !is_in_allowed_fast(self.pieces[i].index() as u32) {
                let piece = self.pieces.remove(i).unwrap();
                removed_indices.push(piece.index() as u32);
            } else {
                i += 1;
            }
        }

        removed_indices
    }

    /// Return the indices of all target pieces.
    ///
    /// Mirrors C++ `getTargetPieceIndexes()`.
    pub fn get_target_piece_indexes(&self) -> Vec<u32> {
        self.pieces.iter().map(|p| p.index() as u32).collect()
    }
}
