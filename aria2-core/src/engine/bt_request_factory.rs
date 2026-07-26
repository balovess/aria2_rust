//! BT Request Factory — generates Request messages for missing blocks
//!
//! This module implements the C++ `DefaultBtRequestFactory` architecture,
//! managing a per-peer target piece list and generating Request messages
//! for blocks that need to be downloaded.
//!
//! # C++ Architecture Reference
//!
//! - `src/DefaultBtRequestFactory.h/.cc` — Request message factory
//! - `src/BtRequestFactory.h` — Abstract interface
//!
//! # Key Differences from C++
//!
//! - C++ stores `shared_ptr<Piece>` and interacts with `BtMessageDispatcher`
//!   and `BtMessageFactory` via raw pointers. Rust uses `Piece` by value
//!   (with `PartialEq` by index) and returns `PieceBlockRequest` data
//!   structures instead of serialized messages.
//! - C++ `doChokedAction()` uses `Peer::isInPeerAllowedIndexSet()`.
//!   Rust passes a closure `is_in_allowed_fast(index) -> bool`.
//! - C++ `createRequestMessagesOnEndGame()` uses `Piece::getAllMissingBlockIndexes()`
//!   + `BtMessageDispatcher::isOutstandingRequest()`. Rust uses
//!   `Piece::missing_block_bitfield_bytes()` + a closure for outstanding checks.
//! - C++ relies on `BtMessageDispatcher::doAbortOutstandingRequestAction()`.
//!   Rust returns piece indices so the caller can perform the abort.

use std::collections::VecDeque;

use rand::seq::SliceRandom;
use tracing::{debug, trace};

use crate::segment::piece::Piece;

/// Lightweight request descriptor returned by the factory.
///
/// Contains the three fields of a BT Request message payload:
/// `index` (piece index), `begin` (byte offset within piece), `length` (block length).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PieceBlockRequest {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

// ======================================================================
// PieceStorageProvider trait — abstraction for piece_storage dependency
// ======================================================================

/// Trait abstracting the piece storage operations needed by BtRequestFactory.
///
/// In C++ `DefaultBtRequestFactory`, `pieceStorage_` is a raw pointer to
/// `PieceStorage`, used only for `cancelPiece()`. This trait exposes just
/// that one operation so the factory remains decoupled from the full
/// `PieceStorage` trait.
pub trait PieceStorageProvider: Send + Sync {
    /// Cancel a piece download for the given CUID.
    /// Mirrors C++ `PieceStorage::cancelPiece()`.
    fn cancel_piece(&self, piece_index: usize, cuid: u64);
}

// ======================================================================
// BtRequestFactory
// ======================================================================

/// BitTorrent request factory, matching C++ `DefaultBtRequestFactory`.
///
/// Manages a per-peer target piece list and generates Request messages
/// for missing blocks. Two modes of operation:
///
/// 1. **Normal mode** — requests missing+unused blocks (not in-use by
///    another peer), marking them as in-use before returning.
/// 2. **Endgame mode** — requests all missing blocks regardless of in-use
///    status, skipping blocks that already have an outstanding request.
///    Block order is shuffled to distribute load across peers.
///
/// # Cancellation Protocol
///
/// When a piece is removed (via `remove_target_piece()`, `remove_all_target_pieces()`,
/// or `do_choked_action()`), the factory returns the affected piece indices so
/// the caller can:
/// 1. Call `BtMessageDispatcher::do_abort_outstanding_request_action()` to
///    remove matching request slots and invalidate queued Request messages.
/// 2. The factory itself calls `PieceStorageProvider::cancel_piece()` to
///    release the piece back to the pool.
pub struct BtRequestFactory {
    /// Target pieces assigned to this peer (C++ `pieces_`).
    pieces: VecDeque<Piece>,
    /// Piece storage provider for cancel_piece operations (C++ `pieceStorage_`).
    piece_storage: Option<Box<dyn PieceStorageProvider>>,
    /// Block size for calculating byte offsets (C++ uses Piece::getBlockLength()).
    /// Kept for API compatibility; actual block lengths come from Piece.
    #[allow(dead_code)]
    block_size: u32,
    /// Command ID for logging and cancellation (C++ `cuid_`).
    cuid: u64,
}

impl BtRequestFactory {
    /// Create a new factory with the given block size.
    ///
    /// Typically `block_size` is 16384 (16 KiB), matching `BT_BLOCK_SIZE`.
    pub fn new(block_size: u32) -> Self {
        Self {
            pieces: VecDeque::new(),
            piece_storage: None,
            block_size,
            cuid: 0,
        }
    }

    /// Set the piece storage provider for cancel_piece operations.
    pub fn set_piece_storage(&mut self, storage: Box<dyn PieceStorageProvider>) {
        self.piece_storage = Some(storage);
    }

    /// Set the command ID for logging and cancellation.
    pub fn set_cuid(&mut self, cuid: u64) {
        self.cuid = cuid;
    }

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

    /// Create Request messages for missing blocks.
    ///
    /// Mirrors C++ `createRequestMessages(max, endGame)`.
    ///
    /// # Arguments
    ///
    /// * `max_count` — Maximum number of requests to generate.
    /// * `is_endgame` — If true, use endgame mode (request all missing blocks,
    ///   skip outstanding, shuffle order).
    /// * `is_outstanding` — Closure that returns true if a block already has
    ///   an outstanding request (used in endgame mode). Signature:
    ///   `fn(piece_index: u32, block_index: u32) -> bool`.
    ///
    /// # Returns
    ///
    /// A vector of `PieceBlockRequest` descriptors containing
    /// `(index, begin, length)` for each block to request.
    pub fn create_request_messages(
        &mut self,
        max_count: usize,
        is_endgame: bool,
        is_outstanding: impl Fn(u32, u32) -> bool,
    ) -> Vec<PieceBlockRequest> {
        if is_endgame {
            self.create_request_messages_on_end_game(max_count, &is_outstanding)
        } else {
            self.create_request_messages_normal(max_count)
        }
    }

    /// Normal mode: request missing+unused blocks.
    ///
    /// Mirrors C++ `createRequestMessages()` (non-endgame path).
    /// Iterates through target pieces, finds missing unused blocks,
    /// marks them as in-use, and generates request descriptors.
    fn create_request_messages_normal(&mut self, max_count: usize) -> Vec<PieceBlockRequest> {
        let mut requests = Vec::with_capacity(max_count);
        let mut remaining = max_count;

        for piece in &mut self.pieces {
            if remaining == 0 {
                break;
            }

            // Get missing unused block indexes (marks them as in-use)
            let block_indexes = piece.get_missing_unused_block_indexes(remaining);

            for block_index in &block_indexes {
                let begin = *block_index as u32 * piece.block_length();
                let length = piece.block_length_at(*block_index);

                trace!(
                    "BtRequestFactory: creating request index={}, begin={}, blockIndex={}",
                    piece.index(),
                    begin,
                    block_index
                );

                requests.push(PieceBlockRequest {
                    index: piece.index() as u32,
                    begin,
                    length,
                });
            }

            remaining -= block_indexes.len();
        }

        requests
    }

    /// Endgame mode: request all missing blocks, skip outstanding, shuffle.
    ///
    /// Mirrors C++ `createRequestMessagesOnEndGame()`.
    /// Iterates through target pieces, collects all missing block indexes
    /// (regardless of in-use status), shuffles them, then generates requests
    /// for blocks that don't already have an outstanding request.
    fn create_request_messages_on_end_game(
        &mut self,
        max_count: usize,
        is_outstanding: &impl Fn(u32, u32) -> bool,
    ) -> Vec<PieceBlockRequest> {
        let mut requests = Vec::with_capacity(max_count);
        let mut rng = rand::thread_rng();

        for piece in &self.pieces {
            if requests.len() >= max_count {
                break;
            }

            // Get all missing block indexes (using bitfield bytes)
            let bitfield_bytes = piece.missing_block_bitfield_bytes();
            let num_blocks = piece.count_blocks();

            // Decode the bitfield into block indexes
            let mut missing_block_indexes = Vec::new();
            let mut block_index: usize = 0;
            for &byte in &bitfield_bytes {
                let mut mask: u8 = 0x80; // MSB first
                for _ in 0..8 {
                    if block_index >= num_blocks {
                        break;
                    }
                    if byte & mask != 0 {
                        missing_block_indexes.push(block_index);
                    }
                    mask >>= 1;
                    block_index += 1;
                }
            }

            // Shuffle to distribute requests across peers
            missing_block_indexes.shuffle(&mut rng);

            for block_index in missing_block_indexes {
                if requests.len() >= max_count {
                    break;
                }

                // Skip blocks that already have an outstanding request
                if is_outstanding(piece.index() as u32, block_index as u32) {
                    continue;
                }

                let begin = block_index as u32 * piece.block_length();
                let length = piece.block_length_at(block_index);

                trace!(
                    "BtRequestFactory: creating endgame request index={}, begin={}, blockIndex={}",
                    piece.index(),
                    begin,
                    block_index
                );

                requests.push(PieceBlockRequest {
                    index: piece.index() as u32,
                    begin,
                    length,
                });
            }
        }

        requests
    }

    /// Return the indices of all target pieces.
    ///
    /// Mirrors C++ `getTargetPieceIndexes()`.
    pub fn get_target_piece_indexes(&self) -> Vec<u32> {
        self.pieces.iter().map(|p| p.index() as u32).collect()
    }
}

// ======================================================================
// Tests
// ======================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::segment::piece::Piece;
    use std::sync::Arc;

    // ── Mock PieceStorageProvider for testing ─────────────────────────────

    /// Mock piece storage provider that tracks cancel_piece calls.
    #[derive(Debug)]
    struct MockPieceStorage {
        cancelled: Arc<std::sync::Mutex<Vec<(usize, u64)>>>,
    }

    impl MockPieceStorage {
        fn new() -> Self {
            Self {
                cancelled: Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn cancelled_pieces(&self) -> Vec<(usize, u64)> {
            self.cancelled.lock().unwrap().clone()
        }
    }

    impl PieceStorageProvider for MockPieceStorage {
        fn cancel_piece(&self, piece_index: usize, cuid: u64) {
            self.cancelled.lock().unwrap().push((piece_index, cuid));
        }
    }

    // ── Helper: create a piece with the given index and length ────────────

    fn make_piece(index: usize, length: u64) -> Piece {
        Piece::new(index, length)
    }

    // ── Construction tests ────────────────────────────────────────────────

    #[test]
    fn test_new_factory() {
        let factory = BtRequestFactory::new(16384);
        assert_eq!(factory.count_target_piece(), 0);
        assert_eq!(factory.count_missing_block(), 0);
        assert!(factory.get_target_piece_indexes().is_empty());
    }

    #[test]
    fn test_set_cuid() {
        let mut factory = BtRequestFactory::new(16384);
        factory.set_cuid(42);
        assert_eq!(factory.cuid, 42);
    }

    // ── Add/remove target piece tests ─────────────────────────────────────

    #[test]
    fn test_add_target_piece() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));
        assert_eq!(factory.count_target_piece(), 2);
        assert_eq!(factory.get_target_piece_indexes(), vec![0, 1]);
    }

    #[test]
    fn test_remove_target_piece() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));
        factory.add_target_piece(make_piece(2, 65536));

        let removed = factory.remove_target_piece(1);
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().index(), 1);
        assert_eq!(factory.count_target_piece(), 2);
        assert_eq!(factory.get_target_piece_indexes(), vec![0, 2]);
    }

    #[test]
    fn test_remove_target_piece_not_found() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        let removed = factory.remove_target_piece(99);
        assert!(removed.is_none());
        assert_eq!(factory.count_target_piece(), 1);
    }

    #[test]
    fn test_remove_target_piece_cancels_in_storage() {
        let mut factory = BtRequestFactory::new(16384);
        let mock_storage = MockPieceStorage::new();
        factory.set_piece_storage(Box::new(MockPieceStorage {
            cancelled: mock_storage.cancelled.clone(),
        }));
        factory.set_cuid(42);

        factory.add_target_piece(make_piece(5, 65536));
        factory.remove_target_piece(5);

        let cancelled = mock_storage.cancelled_pieces();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0], (5, 42));
    }

    #[test]
    fn test_remove_all_target_pieces() {
        let mut factory = BtRequestFactory::new(16384);
        let mock_storage = MockPieceStorage::new();
        factory.set_piece_storage(Box::new(MockPieceStorage {
            cancelled: mock_storage.cancelled.clone(),
        }));
        factory.set_cuid(10);

        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));
        factory.add_target_piece(make_piece(2, 65536));

        let removed = factory.remove_all_target_pieces();
        assert_eq!(removed.len(), 3);
        assert_eq!(factory.count_target_piece(), 0);

        // All pieces should be cancelled in storage
        let cancelled = mock_storage.cancelled_pieces();
        assert_eq!(cancelled.len(), 3);
        assert_eq!(cancelled[0], (0, 10));
        assert_eq!(cancelled[1], (1, 10));
        assert_eq!(cancelled[2], (2, 10));
    }

    #[test]
    fn test_remove_all_target_pieces_empty() {
        let mut factory = BtRequestFactory::new(16384);
        let removed = factory.remove_all_target_pieces();
        assert!(removed.is_empty());
    }

    // ── Count missing block tests ─────────────────────────────────────────

    #[test]
    fn test_count_missing_block_empty() {
        let factory = BtRequestFactory::new(16384);
        assert_eq!(factory.count_missing_block(), 0);
    }

    #[test]
    fn test_count_missing_block_aggregation() {
        let mut factory = BtRequestFactory::new(16384);
        // Two pieces with 4 blocks each = 8 missing blocks
        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));
        assert_eq!(factory.count_missing_block(), 8);
    }

    #[test]
    fn test_count_missing_block_after_partial_completion() {
        let mut factory = BtRequestFactory::new(16384);
        let mut piece = make_piece(0, 65536);
        piece.complete_block(0); // 1 of 4 blocks complete
        piece.complete_block(1); // 2 of 4 blocks complete
        factory.add_target_piece(piece);
        assert_eq!(factory.count_missing_block(), 2);
    }

    // ── Remove completed piece tests ──────────────────────────────────────

    #[test]
    fn test_remove_completed_piece() {
        let mut factory = BtRequestFactory::new(16384);

        let mut piece0 = make_piece(0, 65536);
        piece0.complete_block(0);
        piece0.complete_block(1);
        piece0.complete_block(2);
        piece0.complete_block(3);

        let piece1 = make_piece(1, 65536); // Not complete

        factory.add_target_piece(piece0);
        factory.add_target_piece(piece1);

        let removed = factory.remove_completed_piece();
        assert_eq!(removed, vec![0]);
        assert_eq!(factory.count_target_piece(), 1);
        assert_eq!(factory.get_target_piece_indexes(), vec![1]);
    }

    #[test]
    fn test_remove_completed_piece_none_complete() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        let removed = factory.remove_completed_piece();
        assert!(removed.is_empty());
        assert_eq!(factory.count_target_piece(), 1);
    }

    #[test]
    fn test_remove_completed_piece_all_complete() {
        let mut factory = BtRequestFactory::new(16384);

        let mut piece0 = make_piece(0, 65536);
        let mut piece1 = make_piece(1, 65536);
        // Complete all blocks
        for i in 0..4 {
            piece0.complete_block(i);
            piece1.complete_block(i);
        }

        factory.add_target_piece(piece0);
        factory.add_target_piece(piece1);

        let removed = factory.remove_completed_piece();
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&0));
        assert!(removed.contains(&1));
        assert_eq!(factory.count_target_piece(), 0);
    }

    // ── do_choked_action tests ────────────────────────────────────────────

    #[test]
    fn test_do_choked_action_removes_non_allowed() {
        let mut factory = BtRequestFactory::new(16384);
        let mock_storage = MockPieceStorage::new();
        factory.set_piece_storage(Box::new(MockPieceStorage {
            cancelled: mock_storage.cancelled.clone(),
        }));
        factory.set_cuid(5);

        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));
        factory.add_target_piece(make_piece(2, 65536));

        // Only piece 1 is in allowed-fast
        let removed = factory.do_choked_action(|idx| idx == 1);
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&0));
        assert!(removed.contains(&2));
        assert_eq!(factory.count_target_piece(), 1);
        assert_eq!(factory.get_target_piece_indexes(), vec![1]);

        // Cancelled in storage
        let cancelled = mock_storage.cancelled_pieces();
        assert_eq!(cancelled.len(), 2);
    }

    #[test]
    fn test_do_choked_action_all_allowed() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));

        let removed = factory.do_choked_action(|_| true);
        assert!(removed.is_empty());
        assert_eq!(factory.count_target_piece(), 2);
    }

    #[test]
    fn test_do_choked_action_none_allowed() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));

        let removed = factory.do_choked_action(|_| false);
        assert_eq!(removed.len(), 2);
        assert_eq!(factory.count_target_piece(), 0);
    }

    // ── create_request_messages normal mode tests ─────────────────────────

    #[test]
    fn test_create_request_messages_normal() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

        let requests = factory.create_request_messages(2, false, |_, _| false);
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].index, 0);
        assert_eq!(requests[0].begin, 0);
        assert_eq!(requests[0].length, 16384);
        assert_eq!(requests[1].index, 0);
        assert_eq!(requests[1].begin, 16384);
        assert_eq!(requests[1].length, 16384);
    }

    #[test]
    fn test_create_request_messages_normal_multiple_pieces() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 32768)); // 2 blocks
        factory.add_target_piece(make_piece(1, 32768)); // 2 blocks

        let requests = factory.create_request_messages(3, false, |_, _| false);
        assert_eq!(requests.len(), 3);
        // First 2 from piece 0
        assert_eq!(requests[0].index, 0);
        assert_eq!(requests[1].index, 0);
        // Third from piece 1
        assert_eq!(requests[2].index, 1);
    }

    #[test]
    fn test_create_request_messages_max_count_zero() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));

        let requests = factory.create_request_messages(0, false, |_, _| false);
        assert!(requests.is_empty());
    }

    #[test]
    fn test_create_request_messages_marks_blocks_in_use() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

        // First call: request 2 blocks
        let requests = factory.create_request_messages(2, false, |_, _| false);
        assert_eq!(requests.len(), 2);

        // Second call: blocks 0 and 1 are already in-use, so we get blocks 2 and 3
        let requests2 = factory.create_request_messages(2, false, |_, _| false);
        assert_eq!(requests2.len(), 2);
        assert_eq!(requests2[0].begin, 32768);
        assert_eq!(requests2[1].begin, 49152);

        // Third call: all blocks are either completed or in-use
        let requests3 = factory.create_request_messages(2, false, |_, _| false);
        assert!(requests3.is_empty());
    }

    #[test]
    fn test_create_request_messages_empty_factory() {
        let mut factory = BtRequestFactory::new(16384);
        let requests = factory.create_request_messages(10, false, |_, _| false);
        assert!(requests.is_empty());
    }

    #[test]
    fn test_create_request_messages_all_blocks_in_use() {
        let mut factory = BtRequestFactory::new(16384);
        let mut piece = make_piece(0, 32768); // 2 blocks
        // Mark both blocks as in-use
        piece.set_block_in_use(0);
        piece.set_block_in_use(1);
        factory.add_target_piece(piece);

        let requests = factory.create_request_messages(2, false, |_, _| false);
        assert!(requests.is_empty());
    }

    // ── create_request_messages endgame mode tests ────────────────────────

    #[test]
    fn test_create_request_messages_endgame() {
        let mut factory = BtRequestFactory::new(16384);
        let mut piece = make_piece(0, 65536); // 4 blocks
        // Complete block 0, leave blocks 1-3 missing
        piece.complete_block(0);
        factory.add_target_piece(piece);

        // In endgame mode, missing blocks 1-3 should be requested
        // (blocks 1-3 are all "missing" even if in-use)
        let requests = factory.create_request_messages(10, true, |_, _| false);
        // Should get requests for the 3 missing blocks
        assert_eq!(requests.len(), 3);
        // Verify all are for piece 0
        for req in &requests {
            assert_eq!(req.index, 0);
        }
    }

    #[test]
    fn test_create_request_messages_endgame_skips_outstanding() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

        // Block 1 is outstanding (already requested by another peer)
        let requests = factory.create_request_messages(10, true, |piece_idx, block_idx| {
            piece_idx == 0 && block_idx == 1
        });

        // Should get requests for blocks 0, 2, 3 (block 1 is outstanding)
        assert_eq!(requests.len(), 3);
        let requested_blocks: Vec<u32> = requests.iter().map(|r| r.begin / 16384).collect();
        assert!(!requested_blocks.contains(&1));
    }

    #[test]
    fn test_create_request_messages_endgame_max_count() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

        let requests = factory.create_request_messages(2, true, |_, _| false);
        assert_eq!(requests.len(), 2);
    }

    #[test]
    fn test_create_request_messages_endgame_all_outstanding() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536)); // 4 blocks

        // All blocks are outstanding
        let requests = factory.create_request_messages(10, true, |_, _| true);
        assert!(requests.is_empty());
    }

    #[test]
    fn test_create_request_messages_endgame_does_not_mark_in_use() {
        // In endgame mode, blocks are NOT marked as in-use on the Piece,
        // because multiple peers may request the same block.
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 32768)); // 2 blocks

        let _ = factory.create_request_messages(10, true, |_, _| false);

        // Verify the blocks are NOT marked as in-use (endgame doesn't mark)
        // We check by calling normal mode and seeing all blocks are still available
        let requests = factory.create_request_messages(2, false, |_, _| false);
        assert_eq!(requests.len(), 2);
    }

    // ── Edge case tests ───────────────────────────────────────────────────

    #[test]
    fn test_get_target_piece_indexes() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(5, 65536));
        factory.add_target_piece(make_piece(10, 65536));
        factory.add_target_piece(make_piece(3, 65536));

        let indexes = factory.get_target_piece_indexes();
        assert_eq!(indexes, vec![5, 10, 3]); // Order preserved (FIFO)
    }

    #[test]
    fn test_create_request_messages_piece_with_non_aligned_length() {
        let mut factory = BtRequestFactory::new(16384);
        // 50000 bytes = 4 blocks (last block is 848 bytes)
        factory.add_target_piece(make_piece(0, 50000));

        let requests = factory.create_request_messages(4, false, |_, _| false);
        assert_eq!(requests.len(), 4);
        // Last block length should be 848
        assert_eq!(requests[3].length, 848);
        assert_eq!(requests[3].begin, 49152);
    }

    #[test]
    fn test_count_target_piece() {
        let mut factory = BtRequestFactory::new(16384);
        assert_eq!(factory.count_target_piece(), 0);
        factory.add_target_piece(make_piece(0, 65536));
        assert_eq!(factory.count_target_piece(), 1);
        factory.add_target_piece(make_piece(1, 65536));
        assert_eq!(factory.count_target_piece(), 2);
        factory.remove_target_piece(0);
        assert_eq!(factory.count_target_piece(), 1);
    }

    #[test]
    fn test_remove_all_then_add() {
        let mut factory = BtRequestFactory::new(16384);
        factory.add_target_piece(make_piece(0, 65536));
        factory.add_target_piece(make_piece(1, 65536));
        factory.remove_all_target_pieces();
        assert_eq!(factory.count_target_piece(), 0);

        factory.add_target_piece(make_piece(2, 65536));
        assert_eq!(factory.count_target_piece(), 1);
        assert_eq!(factory.get_target_piece_indexes(), vec![2]);
    }

    #[test]
    fn test_do_choked_action_empty_factory() {
        let mut factory = BtRequestFactory::new(16384);
        let removed = factory.do_choked_action(|_| false);
        assert!(removed.is_empty());
    }

    #[test]
    fn test_create_request_messages_endgame_empty_factory() {
        let mut factory = BtRequestFactory::new(16384);
        let requests = factory.create_request_messages(10, true, |_, _| false);
        assert!(requests.is_empty());
    }
}
