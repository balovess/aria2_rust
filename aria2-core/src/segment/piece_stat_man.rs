//! Piece Statistics Manager for BitTorrent rarest-first piece selection.
//!
//! Tracks per-piece peer counts (how many peers have each piece) and maintains
//! a random-shuffled order array used for tie-breaking when multiple pieces
//! share the same rarity level.
//!
//! # Concurrency
//!
//! `PieceStatMan` is designed to be shared via `Arc` across the BitTorrent
//! subsystem. The `order` array is immutable after construction, while the
//! `counts` vector uses `RwLock` for interior mutability, allowing concurrent
//! reads during piece selection and exclusive writes when peer bitfields change.
//!
//! # Algorithm
//!
//! The rarest-first strategy prefers pieces that fewer peers possess, maximizing
//! piece diversity across the swarm. When two pieces have the same count, the
//! `order` array (initialized with a Fisher-Yates shuffle) breaks ties
//! deterministically for a given session, avoiding thundering-herd effects.
//!
//! # C++ Reference
//!
//! Ported from `aria2_original/src/PieceStatMan.h` and `.cc`.

use std::sync::RwLock;

use rand::seq::SliceRandom;
use tracing::trace;

use super::bitfield_util::{byte_at, for_each_set_bit, for_each_set_byte};

// ---------------------------------------------------------------------------
// PieceStatMan
// ---------------------------------------------------------------------------

/// Manages piece frequency statistics for rarest-first piece selection.
///
/// - `order`: piece indices in a random-shuffled order (tie-breaker).
///   Immutable after construction — safe for concurrent reads without locking.
/// - `counts`: per-piece peer count (saturating `u32`), protected by `RwLock`
///   for interior mutability when shared via `Arc`.
pub struct PieceStatMan {
    /// Piece indices in random-shuffled order for tie-breaking.
    /// Never mutated after construction, so no lock needed.
    order: Vec<u32>,
    /// Per-piece peer count (how many peers possess this piece).
    /// Protected by RwLock for concurrent read / exclusive write.
    counts: RwLock<Vec<u32>>,
}

impl PieceStatMan {
    /// Create a new `PieceStatMan` for `piece_num` pieces.
    ///
    /// If `random_shuffle` is true, the `order` array is shuffled using a
    /// Fisher-Yates shuffle via `rand::thread_rng()`, providing per-session
    /// randomization for tie-breaking.  When false, `order` is simply
    /// `[0, 1, 2, ..., piece_num-1]`.
    pub fn new(piece_num: usize, random_shuffle: bool) -> Self {
        // Build the order array; shuffle if requested (before moving into struct,
        // since `order` is immutable after construction and not behind a lock).
        let mut order: Vec<u32> = (0..piece_num)
            .map(|index| u32::try_from(index).expect("piece count exceeds u32::MAX"))
            .collect();
        if random_shuffle {
            let mut rng = rand::thread_rng();
            order.shuffle(&mut rng);
            trace!(
                piece_num,
                "PieceStatMan created with random shuffle, order randomized"
            );
        } else {
            trace!(piece_num, "PieceStatMan created without shuffle");
        }

        PieceStatMan {
            order,
            counts: RwLock::new(vec![0u32; piece_num]),
        }
    }

    /// Increment the peer count for a single piece by index.
    ///
    /// Saturates at `u32::MAX` (matching C++ `inc` with `INT_MAX` guard).
    pub fn add_piece_stats_index(&self, index: usize) {
        let mut counts = self.counts.write().unwrap();
        if index < counts.len() {
            counts[index] = counts[index].saturating_add(1);
            trace!(index, count = counts[index], "add_piece_stats_index");
        }
    }

    /// Increment peer counts for all pieces whose bits are set in `bitfield`.
    ///
    /// The bitfield uses MSB-first ordering and encodes `counts.len()`
    /// total bits.  Each set bit causes the corresponding piece count to
    /// increment (saturating at `u32::MAX`).
    pub fn add_piece_stats_bitfield(&self, bitfield: &[u8]) {
        let mut counts = self.counts.write().unwrap();
        let nbits = counts.len();
        for_each_set_bit(bitfield, nbits, |index| {
            counts[index] = counts[index].saturating_add(1);
        });
        trace!(nbits, "add_piece_stats_bitfield completed");
    }

    /// Decrement peer counts for all pieces whose bits are set in `bitfield`.
    ///
    /// Saturates at 0 (matching C++ `sub` with `> 0` guard).
    pub fn subtract_piece_stats(&self, bitfield: &[u8]) {
        let mut counts = self.counts.write().unwrap();
        let nbits = counts.len();
        for_each_set_bit(bitfield, nbits, |index| {
            counts[index] = counts[index].saturating_sub(1);
        });
        trace!(nbits, "subtract_piece_stats completed");
    }

    /// Update peer counts based on the diff between `new_bitfield` and
    /// `old_bitfield`.
    ///
    /// - Bits set in `new` but not in `old`: increment (peer gained a piece).
    /// - Bits set in `old` but not in `new`: decrement (peer lost a piece).
    /// - Bits set in both or neither: no change.
    ///
    /// This is more efficient than calling `subtract` then `add` because it
    /// only touches pieces that actually changed.
    pub fn update_piece_stats(&self, new_bitfield: &[u8], old_bitfield: &[u8]) {
        let mut counts = self.counts.write().unwrap();
        let nbits = counts.len();
        for byte_index in 0..nbits.div_ceil(8) {
            let added = byte_at(new_bitfield, byte_index) & !byte_at(old_bitfield, byte_index);
            for_each_set_byte(added, byte_index, nbits, &mut |index| {
                counts[index] = counts[index].saturating_add(1);
            });
            let removed = byte_at(old_bitfield, byte_index) & !byte_at(new_bitfield, byte_index);
            for_each_set_byte(removed, byte_index, nbits, &mut |index| {
                counts[index] = counts[index].saturating_sub(1);
            });
        }
        trace!(nbits, "update_piece_stats completed");
    }

    /// Returns the piece order array (random-shuffled indices for tie-breaking).
    #[inline]
    pub fn order(&self) -> &[u32] {
        &self.order
    }

    /// Returns a cloned snapshot of the per-piece peer count vector.
    ///
    /// This acquires a read lock and clones the data to avoid holding the
    /// lock across the caller's usage. For performance-sensitive reads
    /// (e.g. piece selection), prefer [`counts_ref`] which returns a
    /// lock guard.
    pub fn counts_snapshot(&self) -> Vec<u32> {
        self.counts.read().unwrap().clone()
    }

    /// Acquires a read lock on the counts and returns a guard.
    ///
    /// Use this when you need to inspect counts without copying, but be
    /// careful not to hold the guard across an `.await` point or while
    /// calling mutation methods (which would deadlock).
    #[inline]
    pub fn counts_ref(&self) -> std::sync::RwLockReadGuard<'_, Vec<u32>> {
        self.counts.read().unwrap()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Construction -------------------------------------------------------

    #[test]
    fn test_new_without_shuffle() {
        let man = PieceStatMan::new(10, false);
        assert_eq!(man.order(), &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(*man.counts_ref(), &[0u32; 10]);
    }

    #[test]
    fn test_new_with_shuffle() {
        let man = PieceStatMan::new(100, true);
        // Order must be a permutation of 0..100
        let mut sorted: Vec<u32> = man.order().to_vec();
        sorted.sort();
        assert_eq!(sorted, (0..100u32).collect::<Vec<_>>());
        // Extremely unlikely that shuffle produces identity
        assert_ne!(man.order(), (0..100u32).collect::<Vec<_>>());
    }

    #[test]
    fn test_new_zero_pieces() {
        let man = PieceStatMan::new(0, false);
        assert!(man.order().is_empty());
        assert!(man.counts_ref().is_empty());
    }

    // -- Single index increment ---------------------------------------------

    #[test]
    fn test_add_piece_stats_index() {
        let man = PieceStatMan::new(5, false);
        man.add_piece_stats_index(2);
        man.add_piece_stats_index(2);
        man.add_piece_stats_index(4);
        assert_eq!(*man.counts_ref(), &[0, 0, 2, 0, 1]);
    }

    #[test]
    fn test_add_piece_stats_index_out_of_bounds() {
        let man = PieceStatMan::new(3, false);
        // Should silently ignore out-of-bounds index
        man.add_piece_stats_index(99);
        assert_eq!(*man.counts_ref(), &[0, 0, 0]);
    }

    // -- Bitfield add -------------------------------------------------------

    #[test]
    fn test_add_piece_stats_bitfield() {
        let man = PieceStatMan::new(8, false);
        // 0b10110001 in MSB-first: bits 0,2,3,7 set (128+32+16+1)
        let bitfield: &[u8] = &[0b10110001];
        man.add_piece_stats_bitfield(bitfield);
        assert_eq!(*man.counts_ref(), &[1, 0, 1, 1, 0, 0, 0, 1]);
    }

    #[test]
    fn test_add_piece_stats_bitfield_multi_byte() {
        let man = PieceStatMan::new(16, false);
        // byte 0: 0b11000000 (bits 0,1), byte 1: 0b00000011 (bits 14,15)
        let bitfield: &[u8] = &[0b11000000, 0b00000011];
        man.add_piece_stats_bitfield(bitfield);
        let counts = man.counts_ref();
        assert_eq!(counts[0], 1);
        assert_eq!(counts[1], 1);
        assert_eq!(counts[14], 1);
        assert_eq!(counts[15], 1);
        // Middle bits should be 0
        for i in 2..14 {
            assert_eq!(counts[i], 0);
        }
    }

    // -- Bitfield subtract --------------------------------------------------

    #[test]
    fn test_subtract_piece_stats() {
        let man = PieceStatMan::new(8, false);
        // Add first — all 8 bits set
        let bitfield: &[u8] = &[0b11111111];
        man.add_piece_stats_bitfield(bitfield);
        assert_eq!(*man.counts_ref(), &[1, 1, 1, 1, 1, 1, 1, 1]);

        // Subtract partial: 0b10100001 = MSB-first bits 0,2,7
        let partial: &[u8] = &[0b10100001];
        man.subtract_piece_stats(partial);
        assert_eq!(*man.counts_ref(), &[0, 1, 0, 1, 1, 1, 1, 0]);
    }

    #[test]
    fn test_subtract_piece_stats_saturating() {
        let man = PieceStatMan::new(4, false);
        // Subtract from zero — should saturate at 0, not underflow
        let bitfield: &[u8] = &[0b11110000]; // bits 0..3 not set
        man.subtract_piece_stats(bitfield);
        assert_eq!(*man.counts_ref(), &[0, 0, 0, 0]);

        // Bits that ARE set in a 4-bit context: 0b10100000 (bits 0 and 2 set)
        let set_bits: &[u8] = &[0b10100000];
        man.subtract_piece_stats(set_bits);
        // Still 0 — saturating sub prevents underflow
        assert_eq!(*man.counts_ref(), &[0, 0, 0, 0]);
    }

    // -- Update piece stats -------------------------------------------------

    #[test]
    fn test_update_piece_stats() {
        let man = PieceStatMan::new(8, false);
        // Old: bits 0, 1 set
        let old_bf: &[u8] = &[0b11000000];
        man.add_piece_stats_bitfield(old_bf);
        assert_eq!(*man.counts_ref(), &[1, 1, 0, 0, 0, 0, 0, 0]);

        // New: bits 0, 2 set (lost bit 1, gained bit 2)
        let new_bf: &[u8] = &[0b10100000];
        man.update_piece_stats(new_bf, old_bf);
        assert_eq!(*man.counts_ref(), &[1, 0, 1, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn test_update_piece_stats_no_change() {
        let man = PieceStatMan::new(8, false);
        let bf: &[u8] = &[0b10101010]; // bits 0, 2, 4, 6
        man.add_piece_stats_bitfield(bf);

        let counts_before = man.counts_snapshot();
        // Update with same bitfield — no change
        man.update_piece_stats(bf, bf);
        assert_eq!(*man.counts_ref(), counts_before.as_slice());
    }

    // -- Saturating behavior at u32::MAX ------------------------------------

    #[test]
    fn test_saturating_add_at_max() {
        let man = PieceStatMan::new(3, false);
        {
            let mut counts = man.counts.write().unwrap();
            counts[1] = u32::MAX;
        }
        man.add_piece_stats_index(1);
        assert_eq!(man.counts_ref()[1], u32::MAX, "should saturate at u32::MAX");
    }

    #[test]
    fn test_saturating_sub_at_zero() {
        let man = PieceStatMan::new(3, false);
        assert_eq!(man.counts_ref()[0], 0);
        man.subtract_piece_stats(&[0b10000000]); // bit 0 set
        assert_eq!(man.counts_ref()[0], 0, "should saturate at 0");
    }

    // -- Order preservation -------------------------------------------------

    #[test]
    fn test_order_unchanged_after_operations() {
        let man = PieceStatMan::new(5, false);
        let original_order = man.order().to_vec();

        man.add_piece_stats_index(0);
        man.add_piece_stats_bitfield(&[0b11111000]); // bits 0-4 in 5-bit context
        man.subtract_piece_stats(&[0b10000000]);
        man.update_piece_stats(&[0b11000000], &[0b10100000]);

        assert_eq!(
            man.order(),
            original_order.as_slice(),
            "order should never change after construction"
        );
    }

    // -- Empty bitfield -----------------------------------------------------

    #[test]
    fn test_empty_bitfield() {
        let man = PieceStatMan::new(8, false);
        let empty: &[u8] = &[0b00000000];
        man.add_piece_stats_bitfield(empty);
        assert_eq!(*man.counts_ref(), &[0u32; 8]);

        man.subtract_piece_stats(empty);
        assert_eq!(*man.counts_ref(), &[0u32; 8]);
    }

    // -- All bits set -------------------------------------------------------

    #[test]
    fn test_all_bits_set() {
        let man = PieceStatMan::new(8, false);
        let all_set: &[u8] = &[0b11111111];
        man.add_piece_stats_bitfield(all_set);
        assert_eq!(*man.counts_ref(), &[1u32; 8]);

        man.subtract_piece_stats(all_set);
        assert_eq!(*man.counts_ref(), &[0u32; 8]);
    }

    #[test]
    fn test_all_bits_set_multi_byte() {
        let man = PieceStatMan::new(16, false);
        let all_set: &[u8] = &[0xFF, 0xFF];
        man.add_piece_stats_bitfield(all_set);
        assert_eq!(*man.counts_ref(), &[1u32; 16]);
    }

    // -- Shared via Arc (concurrent access pattern) -------------------------

    #[test]
    fn test_arc_shared_mutation() {
        use std::sync::Arc;
        let man = Arc::new(PieceStatMan::new(4, false));
        man.add_piece_stats_index(0);
        man.add_piece_stats_index(1);
        man.add_piece_stats_bitfield(&[0b11110000]); // bits 0-3
        assert_eq!(*man.counts_ref(), &[2, 2, 1, 1]);
    }
}
