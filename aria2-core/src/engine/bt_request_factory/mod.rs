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
//!     `Piece::missing_block_bitfield_bytes()` + a closure for outstanding checks.
//! - C++ relies on `BtMessageDispatcher::doAbortOutstandingRequestAction()`.
//!   Rust returns piece indices so the caller can perform the abort.

mod piece_selection;
mod pipeline;
mod tests;

// Sub-modules define `impl BtRequestFactory` blocks; no new public items to re-export.


use std::collections::VecDeque;

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
    pub(crate) pieces: VecDeque<Piece>,
    /// Piece storage provider for cancel_piece operations (C++ `pieceStorage_`).
    pub(crate) piece_storage: Option<Box<dyn PieceStorageProvider>>,
    /// Block size for calculating byte offsets (C++ uses `Piece::getBlockLength()`).
    /// Kept for API compatibility; actual block lengths come from Piece.
    #[allow(dead_code)]
    pub(crate) block_size: u32,
    /// Command ID for logging and cancellation (C++ `cuid_`).
    pub(crate) cuid: u64,
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
}