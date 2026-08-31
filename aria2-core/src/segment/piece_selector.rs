//! Piece selection strategies for BitTorrent downloads.
//!
//! This module implements the piece selection strategy pattern used by BitTorrent
//! for choosing which piece to download next. It replaces the C++ virtual dispatch
//! with an enum-based dispatch pattern for zero-overhead performance.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PieceSelector.h` — Abstract piece selector interface
//! - `src/RarestPieceSelector.h/.cc` — Rarest-first selection
//! - `src/PriorityPieceSelector.h/.cc` — Priority + delegate selection
//!
//! # Design
//!
//! Unlike the C++ version which uses virtual dispatch (`shared_ptr<PieceSelector>`),
//! this Rust version uses enum dispatch (`PieceSelectorKind`) for:
//! - Zero-overhead dispatch (no vtable indirection)
//! - Exhaustive pattern matching
//! - No heap allocation for the selector itself (except `Box` for recursive inner)
//!
//! # Selection Strategies
//!
//! - **Rarest First**: Selects the piece with the lowest availability count
//!   across all connected peers. This improves swarm health by prioritizing
//!   rare pieces.
//! - **Priority**: First checks a list of explicitly prioritized pieces,
//!   then delegates to an inner selector (typically Rarest). Used for
//!   sequential head/tail downloading or user-specified piece priorities.

use std::sync::Arc;
use tracing::trace;

use super::PieceStatMan;
use super::bitfield_util::test_bit;

// ===========================================================================
// RarestPieceSelector
// ===========================================================================

/// Selects the piece with the lowest availability count (rarest-first strategy).
///
/// Iterates through the piece order from [`PieceStatMan`], finding the piece
/// that is available in the peer's bitfield and has the lowest count
/// (i.e., the fewest peers have it). This improves swarm health by
/// ensuring rare pieces are downloaded first.
///
/// # C++ Reference
///
/// Based on `RarestPieceSelector.cc` from aria2.
///
/// # Algorithm
///
/// For each piece index `order[i]` (i = 0..nbits), check if the peer has it
/// (bit set in bitfield) and if its count is lower than the current minimum.
/// Return the piece with the lowest count.
pub struct RarestPieceSelector {
    /// Shared reference to the piece statistics manager
    piece_stat_man: Arc<PieceStatMan>,
}

impl RarestPieceSelector {
    /// Creates a new `RarestPieceSelector` with the given piece statistics manager.
    pub fn new(piece_stat_man: Arc<PieceStatMan>) -> Self {
        RarestPieceSelector { piece_stat_man }
    }

    /// Selects the rarest piece available in the peer's bitfield.
    ///
    /// Returns `Some(index)` if a piece was found, `None` if no piece is available.
    ///
    /// # Arguments
    ///
    /// * `bitfield` — The peer's have-bitfield (MSB-first)
    /// * `nbits` — Number of pieces (bits in the bitfield)
    pub fn select(&self, bitfield: &[u8], nbits: usize) -> Option<usize> {
        let order = self.piece_stat_man.order();
        let counts = self.piece_stat_man.counts_ref();

        // Guard: order and counts must have at least nbits entries
        if order.len() < nbits || counts.len() < nbits {
            trace!(
                order_len = order.len(),
                counts_len = counts.len(),
                nbits,
                "RarestPieceSelector: order/counts shorter than nbits"
            );
            return None;
        }

        let mut min_count = u32::MAX;
        let mut best_idx = nbits; // sentinel: not found

        for &idx in order.iter().take(nbits) {
            let idx = idx as usize;
            if test_bit(bitfield, nbits, idx) && counts[idx] < min_count {
                min_count = counts[idx];
                best_idx = idx;
            }
        }

        if best_idx == nbits {
            trace!(nbits, "RarestPieceSelector: no available piece found");
            None
        } else {
            trace!(
                best_idx,
                min_count, nbits, "RarestPieceSelector: selected piece"
            );
            Some(best_idx)
        }
    }

    /// Returns a reference to the underlying `PieceStatMan`.
    pub fn piece_stat_man(&self) -> &PieceStatMan {
        &self.piece_stat_man
    }
}

// ===========================================================================
// PriorityPieceSelector
// ===========================================================================

/// Selects prioritized pieces first, then delegates to an inner selector.
///
/// This wraps another piece selector and adds a priority list. When [`select`]
/// is called, it first checks if any prioritized piece is available in the
/// peer's bitfield. If so, it returns that piece immediately. Otherwise,
/// it delegates to the inner selector.
///
/// [`select`]: PriorityPieceSelector::select
///
/// # C++ Reference
///
/// Based on `PriorityPieceSelector.cc` from aria2.
///
/// # Inner Selector
///
/// The inner selector is stored as `Box<PieceSelectorKind>` to break the
/// recursive type cycle (`PriorityPieceSelector` contains `PieceSelectorKind`
/// which contains `PriorityPieceSelector`). This is the minimal indirection
/// needed; the C++ version uses `shared_ptr<PieceSelector>` (also a pointer).
pub struct PriorityPieceSelector {
    /// Pieces to prioritize (checked before the inner selector)
    prioritized_pieces: Vec<usize>,
    /// Inner selector to delegate to when no prioritized piece is available
    inner: Box<PieceSelectorKind>,
}

impl PriorityPieceSelector {
    /// Creates a new `PriorityPieceSelector` wrapping the given inner selector.
    ///
    /// The prioritized pieces list is initially empty. Use [`set_priority_pieces`]
    /// to add prioritized pieces.
    ///
    /// [`set_priority_pieces`]: PriorityPieceSelector::set_priority_pieces
    pub fn new(inner: PieceSelectorKind) -> Self {
        PriorityPieceSelector {
            prioritized_pieces: Vec::new(),
            inner: Box::new(inner),
        }
    }

    /// Sets the list of prioritized pieces, replacing any existing list.
    pub fn set_priority_pieces(&mut self, pieces: Vec<usize>) {
        trace!(
            count = pieces.len(),
            "PriorityPieceSelector: setting priority pieces"
        );
        self.prioritized_pieces = pieces;
    }

    /// Returns a reference to the prioritized pieces list.
    pub fn prioritized_pieces(&self) -> &[usize] {
        &self.prioritized_pieces
    }

    /// Selects a piece, checking prioritized pieces first.
    ///
    /// Returns `Some(index)` if a piece was found, `None` if no piece is available.
    ///
    /// # Algorithm
    ///
    /// 1. Iterate through `prioritized_pieces`; if any is set in the bitfield,
    ///    return it immediately.
    /// 2. Otherwise, delegate to the inner selector.
    pub fn select(&self, bitfield: &[u8], nbits: usize) -> Option<usize> {
        for &p in &self.prioritized_pieces {
            if test_bit(bitfield, nbits, p) {
                trace!(
                    index = p,
                    nbits, "PriorityPieceSelector: selected prioritized piece"
                );
                return Some(p);
            }
        }
        trace!(
            nbits,
            "PriorityPieceSelector: no prioritized piece available, delegating"
        );
        self.inner.select(bitfield, nbits)
    }
}

// ===========================================================================
// PieceSelectorKind — Enum dispatch
// ===========================================================================

/// Enum dispatch for piece selection strategies, replacing C++ virtual dispatch.
///
/// The C++ implementation uses a `PieceSelector` base class with virtual methods
/// and `shared_ptr<PieceSelector>`. This Rust version uses an enum for
/// zero-overhead dispatch and exhaustive pattern matching.
///
/// # Variants
///
/// - [`Rarest`](PieceSelectorKind::Rarest) — Rarest-first selection
/// - [`Priority`](PieceSelectorKind::Priority) — Priority + delegate selection
pub enum PieceSelectorKind {
    /// Rarest-first piece selector
    Rarest(RarestPieceSelector),
    /// Priority piece selector (checks priority list, then delegates)
    Priority(PriorityPieceSelector),
}

impl PieceSelectorKind {
    /// Selects the next piece to download.
    ///
    /// Returns `Some(index)` if a piece was found, `None` if no piece is available.
    ///
    /// # Arguments
    ///
    /// * `bitfield` — The peer's have-bitfield (MSB-first)
    /// * `nbits` — Number of pieces (bits in the bitfield)
    pub fn select(&self, bitfield: &[u8], nbits: usize) -> Option<usize> {
        match self {
            PieceSelectorKind::Rarest(r) => r.select(bitfield, nbits),
            PieceSelectorKind::Priority(p) => p.select(bitfield, nbits),
        }
    }
}

impl std::fmt::Debug for PieceSelectorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PieceSelectorKind::Rarest(_) => f.debug_struct("PieceSelectorKind::Rarest").finish(),
            PieceSelectorKind::Priority(p) => f
                .debug_struct("PieceSelectorKind::Priority")
                .field("prioritized_count", &p.prioritized_pieces.len())
                .finish(),
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── Helper ──────────────────────────────────────────────────────────

    /// Build a bitfield byte array from a list of set bit indices (MSB-first).
    fn build_bitfield(nbits: usize, set_bits: &[usize]) -> Vec<u8> {
        let num_bytes = nbits.div_ceil(8);
        let mut bf = vec![0u8; num_bytes];
        for &bit in set_bits {
            let byte = bit / 8;
            let bit_pos = 7 - (bit % 8);
            if byte < bf.len() {
                bf[byte] |= 1 << bit_pos;
            }
        }
        bf
    }

    // ── RarestPieceSelector tests ───────────────────────────────────────

    #[test]
    fn test_rarest_select_basic() {
        // 10 pieces, no random shuffle → order = [0,1,2,...,9]
        let man = Arc::new(PieceStatMan::new(10, false));
        let selector = RarestPieceSelector::new(man);

        // All counts are 0, pieces 0-2 available in bitfield
        let bf = build_bitfield(10, &[0, 1, 2]);
        let result = selector.select(&bf, 10);
        assert!(result.is_some());
        // With identity order and all counts at 0, first available piece wins
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_rarest_select_with_counts() {
        // 10 pieces, no random shuffle
        let man = Arc::new(PieceStatMan::new(10, false));
        // Simulate: piece 0 has 1 peer, others have 0
        man.add_piece_stats_index(0);
        let selector = RarestPieceSelector::new(man);

        // Pieces 0, 1, 2 available; piece 0 has count=1, pieces 1,2 have count=0
        let bf = build_bitfield(10, &[0, 1, 2]);
        let result = selector.select(&bf, 10);
        assert!(result.is_some());
        // Piece 1 (count=0) is rarer than piece 0 (count=1)
        assert_eq!(result.unwrap(), 1);
    }

    #[test]
    fn test_rarest_select_no_available_piece() {
        let man = Arc::new(PieceStatMan::new(10, false));
        let selector = RarestPieceSelector::new(man);

        // Empty bitfield — no pieces available
        let bf = vec![0u8; 2]; // 10 pieces need 2 bytes
        let result = selector.select(&bf, 10);
        assert!(result.is_none());
    }

    #[test]
    fn test_rarest_select_all_bits_set() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let selector = RarestPieceSelector::new(man);

        // All 8 bits set, all counts 0 — first in order wins
        let bf = vec![0xFF];
        let result = selector.select(&bf, 8);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn test_rarest_select_single_piece() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let selector = RarestPieceSelector::new(man);

        // Only piece 5 available
        let bf = build_bitfield(8, &[5]);
        let result = selector.select(&bf, 8);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 5);
    }

    #[test]
    fn test_rarest_select_prefers_lower_count() {
        let man = Arc::new(PieceStatMan::new(8, false));
        // Piece 3 has 5 peers, piece 7 has 1 peer
        for _ in 0..5 {
            man.add_piece_stats_index(3);
        }
        man.add_piece_stats_index(7);
        let selector = RarestPieceSelector::new(man);

        // Both pieces 3 and 7 available
        let bf = build_bitfield(8, &[3, 7]);
        let result = selector.select(&bf, 8);
        assert!(result.is_some());
        // Piece 7 (count=1) is rarer than piece 3 (count=5)
        assert_eq!(result.unwrap(), 7);
    }

    #[test]
    fn test_rarest_select_zero_nbits() {
        let man = Arc::new(PieceStatMan::new(0, false));
        let selector = RarestPieceSelector::new(man);

        let bf: Vec<u8> = vec![];
        let result = selector.select(&bf, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_rarest_select_matching_cpp_test() {
        // Reproduces the C++ RarestPieceSelectorTest::testSelect
        let man = Arc::new(PieceStatMan::new(10, false));
        let selector = RarestPieceSelector::new(Arc::clone(&man));

        // Set bits 0 and 1 in bitfield (10 pieces)
        let bf = build_bitfield(10, &[0, 1]);

        // Add 1 peer for piece 0 → piece 0 count=1, piece 1 count=0
        man.add_piece_stats_index(0);

        // Piece 1 (count=0) is rarer than piece 0 (count=1)
        let result = selector.select(&bf, 10);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 1);

        // Add 1 peer for piece 1 → piece 0 count=1, piece 1 count=1
        man.add_piece_stats_index(1);

        // Now both have count=1; first in order wins → piece 0
        // But wait: both 0 and 1 have count 1. We iterate order[0]=0 first.
        // test_bit(bf, 10, 0) is true, counts[0]=1 < u32::MAX → best_idx=0, min=1
        // test_bit(bf, 10, 1) is true, counts[1]=1 is NOT < 1 → no update
        // So piece 0 wins (first found with lowest count)
        let result = selector.select(&bf, 10);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 0);
    }

    // ── PriorityPieceSelector tests ─────────────────────────────────────

    #[test]
    fn test_priority_select_with_prioritized_pieces() {
        let man = Arc::new(PieceStatMan::new(256, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(man));
        let mut selector = PriorityPieceSelector::new(inner);
        selector.set_priority_pieces(vec![1, 200]);

        // Both pieces 1 and 200 are available
        let bf = build_bitfield(256, &[1, 200]);
        let result = selector.select(&bf, 256);
        assert!(result.is_some());
        // First prioritized piece (1) should be selected
        assert_eq!(result.unwrap(), 1);
    }

    #[test]
    fn test_priority_select_falls_through_when_first_not_available() {
        let man = Arc::new(PieceStatMan::new(256, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(man));
        let mut selector = PriorityPieceSelector::new(inner);
        selector.set_priority_pieces(vec![1, 200]);

        // Only piece 200 is available (piece 1 is not)
        let bf = build_bitfield(256, &[200]);
        let result = selector.select(&bf, 256);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 200);
    }

    #[test]
    fn test_priority_select_delegates_when_no_priority_available() {
        let man = Arc::new(PieceStatMan::new(256, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&man)));
        let mut selector = PriorityPieceSelector::new(inner);
        selector.set_priority_pieces(vec![1, 200]);

        // Neither 1 nor 200 available, but piece 50 is
        let bf = build_bitfield(256, &[50]);
        let result = selector.select(&bf, 256);
        assert!(result.is_some());
        // Should delegate to RarestPieceSelector which selects piece 50
        assert_eq!(result.unwrap(), 50);
    }

    #[test]
    fn test_priority_select_no_prioritized_pieces() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&man)));
        let selector = PriorityPieceSelector::new(inner);

        // No prioritized pieces set — should always delegate
        let bf = build_bitfield(8, &[3]);
        let result = selector.select(&bf, 8);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), 3);
    }

    #[test]
    fn test_priority_select_none_available() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(man));
        let mut selector = PriorityPieceSelector::new(inner);
        selector.set_priority_pieces(vec![1, 2]);

        // Empty bitfield
        let bf = vec![0u8; 1];
        let result = selector.select(&bf, 8);
        assert!(result.is_none());
    }

    #[test]
    fn test_priority_select_set_priority_replaces() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&man)));
        let mut selector = PriorityPieceSelector::new(inner);

        selector.set_priority_pieces(vec![0]);
        let bf = build_bitfield(8, &[0, 3]);
        assert_eq!(selector.select(&bf, 8), Some(0));

        // Replace priority list
        selector.set_priority_pieces(vec![3]);
        assert_eq!(selector.select(&bf, 8), Some(3));
    }

    #[test]
    fn test_priority_nested_priority() {
        // Priority wrapping Priority wrapping Rarest
        let man = Arc::new(PieceStatMan::new(8, false));
        let rarest = PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&man)));

        let mut inner_priority = PriorityPieceSelector::new(rarest);
        inner_priority.set_priority_pieces(vec![2]);

        let mut outer_priority =
            PriorityPieceSelector::new(PieceSelectorKind::Priority(inner_priority));
        outer_priority.set_priority_pieces(vec![0]);

        // Both pieces 0 and 2 available — outer priority takes precedence
        let bf = build_bitfield(8, &[0, 2, 5]);
        assert_eq!(outer_priority.select(&bf, 8), Some(0));

        // Piece 0 not available, piece 2 available — inner priority kicks in
        let bf = build_bitfield(8, &[2, 5]);
        assert_eq!(outer_priority.select(&bf, 8), Some(2));

        // Neither 0 nor 2 available — falls through to Rarest
        let bf = build_bitfield(8, &[5]);
        assert_eq!(outer_priority.select(&bf, 8), Some(5));
    }

    #[test]
    fn test_priority_select_matching_cpp_test() {
        // Reproduces C++ PriorityPieceSelectorTest::testSelect
        let man = Arc::new(PieceStatMan::new(256, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(man));
        let mut selector = PriorityPieceSelector::new(inner);
        selector.set_priority_pieces(vec![1, 200]);

        // Pieces 1 and 200 set in bitfield
        let bf = build_bitfield(256, &[1, 200]);
        assert_eq!(selector.select(&bf, 256), Some(1));

        // Unset piece 1 — should fall through to piece 200
        let bf = build_bitfield(256, &[200]);
        assert_eq!(selector.select(&bf, 256), Some(200));

        // Unset piece 200 too — delegate to Rarest which finds nothing
        // (because only pieces 1 and 200 were set, now neither is)
        let bf = vec![0u8; 32]; // 256 bits, all zero
        assert!(selector.select(&bf, 256).is_none());
    }

    // ── PieceSelectorKind enum dispatch tests ──────────────────────────

    #[test]
    fn test_piece_selector_kind_rarest_dispatch() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let selector = PieceSelectorKind::Rarest(RarestPieceSelector::new(man));

        let bf = build_bitfield(8, &[3]);
        assert_eq!(selector.select(&bf, 8), Some(3));
    }

    #[test]
    fn test_piece_selector_kind_priority_dispatch() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let inner = PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&man)));
        let mut priority = PriorityPieceSelector::new(inner);
        priority.set_priority_pieces(vec![5]);

        let selector = PieceSelectorKind::Priority(priority);
        let bf = build_bitfield(8, &[5]);
        assert_eq!(selector.select(&bf, 8), Some(5));
    }

    #[test]
    fn test_piece_selector_kind_debug_format() {
        let man = Arc::new(PieceStatMan::new(8, false));
        let rarest = PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&man)));
        let debug_str = format!("{:?}", rarest);
        assert!(debug_str.contains("Rarest"));

        let mut priority =
            PriorityPieceSelector::new(PieceSelectorKind::Rarest(RarestPieceSelector::new(man)));
        priority.set_priority_pieces(vec![1, 2, 3]);
        let priority_kind = PieceSelectorKind::Priority(priority);
        let debug_str = format!("{:?}", priority_kind);
        assert!(debug_str.contains("Priority"));
        assert!(debug_str.contains("3")); // prioritized_count
    }
}
