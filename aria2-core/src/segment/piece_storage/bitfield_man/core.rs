//! BitfieldMan core: struct definition, constructor, basic piece/use operations,
//! completion queries, bitfield access, counting, bulk bit ops, range ops,
//! block length queries, and private trailing-bit cleanup helpers.

use super::helpers::{bf_count_set, bf_set, bf_unset};
use crate::segment::bitfield_util::test_bit;

/// Manages completion, usage, and filter bitfields for piece tracking.
///
/// This is the Rust equivalent of the C++ `BitfieldMan` class.
/// It tracks three bitfields:
/// - **completion**: which pieces have been fully downloaded
/// - **use**: which pieces are currently being downloaded (in-flight)
/// - **filter**: which pieces are filtered out (not to be downloaded)
#[derive(Clone)]
pub struct BitfieldMan {
    /// Bitfield tracking completed pieces
    pub(super) bitfield: Vec<u8>,
    /// Bitfield tracking in-use pieces
    pub(super) use_bitfield: Vec<u8>,
    /// Bitfield tracking filtered pieces
    pub(super) filter_bitfield: Vec<u8>,
    /// Number of pieces
    pub(super) num_pieces: usize,
    /// Length of each piece in bytes
    pub(super) piece_length: u64,
    /// Total download length in bytes
    pub(super) total_length: u64,
    /// Cached count of completed pieces
    pub(super) cached_num_piece: usize,
    /// Whether the filter bitfield is enabled
    pub(super) filter_enabled: bool,
}

impl BitfieldMan {
    /// Creates a new BitfieldMan with the given piece length and total length.
    pub fn new(piece_length: u64, total_length: u64) -> Self {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            total_length.div_ceil(piece_length) as usize
        };
        let num_bytes = num_pieces.div_ceil(8);

        BitfieldMan {
            bitfield: vec![0u8; num_bytes],
            use_bitfield: vec![0u8; num_bytes],
            filter_bitfield: vec![0u8; num_bytes],
            num_pieces,
            piece_length,
            total_length,
            cached_num_piece: 0,
            filter_enabled: false,
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// Returns the number of pieces.
    pub fn num_pieces(&self) -> usize {
        self.num_pieces
    }

    /// Returns the piece length in bytes.
    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    /// Returns the total length in bytes.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    // ── Piece completion operations ────────────────────────────────────────

    /// Returns true if the piece at the given index is completed.
    pub fn has_piece(&self, index: usize) -> bool {
        index < self.num_pieces && test_bit(&self.bitfield, self.num_pieces, index)
    }

    /// Sets the piece at the given index as completed.
    pub fn set_piece(&mut self, index: usize) {
        if index < self.num_pieces && !test_bit(&self.bitfield, self.num_pieces, index) {
            bf_set(&mut self.bitfield, index);
            self.cached_num_piece += 1;
        }
    }

    /// Clears the piece at the given index (marks as not completed).
    pub fn clear_piece(&mut self, index: usize) {
        if index < self.num_pieces && test_bit(&self.bitfield, self.num_pieces, index) {
            bf_unset(&mut self.bitfield, index);
            self.cached_num_piece = self.cached_num_piece.saturating_sub(1);
        }
    }

    // ── Piece use (in-flight) operations ──────────────────────────────────

    /// Returns true if the piece is currently in-use (being downloaded).
    pub fn is_use_piece(&self, index: usize) -> bool {
        index < self.num_pieces && test_bit(&self.use_bitfield, self.num_pieces, index)
    }

    /// Marks a piece as in-use (being downloaded).
    pub fn set_use_piece(&mut self, index: usize) {
        if index < self.num_pieces {
            bf_set(&mut self.use_bitfield, index);
        }
    }

    /// Unmarks a piece as in-use.
    pub fn unset_use_piece(&mut self, index: usize) {
        if index < self.num_pieces {
            bf_unset(&mut self.use_bitfield, index);
        }
    }

    // ── Missing piece queries ─────────────────────────────────────────────

    /// Returns true if there are missing pieces that are not in-use.
    pub fn has_missing_piece(&self) -> bool {
        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
            {
                return true;
            }
        }
        false
    }

    /// Returns the index of the first missing unused piece.
    pub fn get_missing_piece_index(&self) -> Option<usize> {
        (0..self.num_pieces).find(|&i| {
            !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
        })
    }

    /// Returns the index of the first missing unused piece that is not
    /// excluded by the ignore bitfield.
    ///
    /// A piece is "ignored" if the corresponding bit is SET in `ignore_bitfield`.
    /// Pieces whose bit is set in the ignore bitfield are skipped.
    pub fn get_missing_piece_index_with_ignore(&self, ignore_bitfield: &[u8]) -> Option<usize> {
        (0..self.num_pieces).find(|&i| {
            !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
                && !test_bit(ignore_bitfield, self.num_pieces, i)
        })
    }

    // ── Completion queries ────────────────────────────────────────────────

    /// Returns the completed length in bytes.
    ///
    /// Correctly handles the last piece which may be shorter than
    /// `piece_length`. Mirrors C++ `BitfieldMan::getCompletedLength()`.
    pub fn get_completed_length(&self) -> u64 {
        if self.num_pieces == 0 {
            return 0;
        }
        if self.cached_num_piece == self.num_pieces {
            return self.total_length;
        }
        // C++ counts piece_length for each completed piece, then adjusts
        // the last piece if it's shorter.
        let last_piece_index = self.num_pieces - 1;
        let last_piece_length = self.total_length - last_piece_index as u64 * self.piece_length;
        let last_piece_is_complete = test_bit(&self.bitfield, self.num_pieces, last_piece_index);

        if last_piece_is_complete {
            // Count full pieces before the last + shorter last piece
            let full_pieces = self.cached_num_piece - 1;
            full_pieces as u64 * self.piece_length + last_piece_length
        } else {
            // All completed pieces are full-length
            self.cached_num_piece as u64 * self.piece_length
        }
    }

    /// Alias for `get_completed_length()` for backward compatibility.
    pub fn get_total_completed_length(&self) -> u64 {
        self.get_completed_length()
    }

    /// Returns true if all pieces are completed.
    pub fn is_all_complete(&self) -> bool {
        self.num_pieces == 0 || self.cached_num_piece == self.num_pieces
    }

    /// Marks pieces as completed up to the given length.
    pub fn mark_pieces_done(&mut self, length: u64) {
        let num_pieces_done = (length / self.piece_length) as usize;
        for i in 0..std::cmp::min(num_pieces_done, self.num_pieces) {
            self.set_piece(i);
        }
    }

    /// Marks all pieces as completed.
    pub fn mark_all_done(&mut self) {
        for i in 0..self.num_pieces {
            self.set_piece(i);
        }
    }

    // ── Bitfield access ───────────────────────────────────────────────────

    /// Returns a reference to the completion bitfield.
    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }

    /// Sets the completion bitfield from external data.
    ///
    /// C++ also clears the use bitfield when setting the completion bitfield.
    /// This ensures stale use bits from a previous session don't persist.
    pub fn set_bitfield(&mut self, bitfield: &[u8]) {
        let copy_len = std::cmp::min(bitfield.len(), self.bitfield.len());
        self.bitfield[..copy_len].copy_from_slice(&bitfield[..copy_len]);
        // C++ clears useBitfield_ on setBitfield()
        for b in self.use_bitfield.iter_mut() {
            *b = 0;
        }
        self.cached_num_piece = bf_count_set(&self.bitfield, self.num_pieces);
    }

    // ── Counting methods ──────────────────────────────────────────────────

    /// Returns the number of remaining (not completed) pieces.
    pub fn count_missing_pieces(&self) -> usize {
        self.num_pieces.saturating_sub(self.cached_num_piece)
    }

    /// Returns the number of filtered pieces that are completed.
    ///
    /// C++: `countFilteredBlock()` — returns `cachedNumFilteredBlock_`.
    /// In Rust, we compute this live since we don't cache it.
    /// When filter is disabled, returns 0 (matching C++).
    pub fn count_filtered_block(&self) -> usize {
        if !self.filter_enabled {
            return 0;
        }
        let mut count = 0usize;
        for i in 0..self.num_pieces {
            if test_bit(&self.bitfield, self.num_pieces, i)
                && test_bit(&self.filter_bitfield, self.num_pieces, i)
            {
                count += 1;
            }
        }
        count
    }

    /// Returns the number of missing filtered pieces.
    ///
    /// C++: `countMissingBlock()` for the filtered case.
    /// When filter is disabled, returns `count_missing_pieces()`.
    pub fn count_missing_filtered_block(&self) -> usize {
        if !self.filter_enabled {
            return self.count_missing_pieces();
        }
        let mut count = 0usize;
        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && test_bit(&self.filter_bitfield, self.num_pieces, i)
            {
                count += 1;
            }
        }
        count
    }

    // ── Bulk bit operations ───────────────────────────────────────────────

    /// Clears all completion bits.
    /// C++: `clearAllBit()` — sets all bits in `bitfield_` to 0.
    pub fn clear_all_bit(&mut self) {
        for byte in &mut self.bitfield {
            *byte = 0;
        }
        self.cached_num_piece = 0;
    }

    /// Sets all completion bits.
    /// C++: `setAllBit()` — marks all pieces as completed.
    pub fn set_all_bit(&mut self) {
        for byte in &mut self.bitfield {
            *byte = 0xFF;
        }
        self.clear_trailing_bits();
        self.cached_num_piece = self.num_pieces;
    }

    /// Clears all use bits.
    /// C++: `clearAllUseBit()` — marks no pieces as in-use.
    pub fn clear_all_use_bit(&mut self) {
        for byte in &mut self.use_bitfield {
            *byte = 0;
        }
    }

    /// Sets all use bits.
    /// C++: `setAllUseBit()` — marks all pieces as in-use.
    pub fn set_all_use_bit(&mut self) {
        for byte in &mut self.use_bitfield {
            *byte = 0xFF;
        }
        self.clear_trailing_use_bits();
    }

    // ── Bit range operations ──────────────────────────────────────────────

    /// Sets bits in the completion bitfield for the range [start, end].
    /// C++: `setBitRange(startIndex, endIndex)` — inclusive on both ends.
    pub fn set_bit_range(&mut self, start_index: usize, end_index: usize) {
        let end = std::cmp::min(end_index, self.num_pieces.saturating_sub(1));
        for i in start_index..=end {
            self.set_piece(i);
        }
    }

    /// Clears bits in the completion bitfield for the range [start, end].
    /// C++: `unsetBitRange(startIndex, endIndex)` — inclusive on both ends.
    pub fn unset_bit_range(&mut self, start_index: usize, end_index: usize) {
        let end = std::cmp::min(end_index, self.num_pieces.saturating_sub(1));
        for i in start_index..=end {
            self.clear_piece(i);
        }
    }

    // ── Block length queries ──────────────────────────────────────────────

    /// Returns the length of the last piece (which may be shorter than piece_length).
    /// C++: `getLastBlockLength()`.
    pub fn get_last_block_length(&self) -> u64 {
        if self.num_pieces == 0 {
            return 0;
        }
        let last_index = self.num_pieces - 1;
        self.total_length - last_index as u64 * self.piece_length
    }

    /// Returns the length of the piece at the given index.
    /// C++: `getBlockLength(index)` — handles the last piece being shorter.
    pub fn get_block_length(&self, index: usize) -> u64 {
        if index >= self.num_pieces {
            return 0;
        }
        if index == self.num_pieces - 1 {
            self.get_last_block_length()
        } else {
            self.piece_length
        }
    }

    /// Returns the maximum valid piece index (num_pieces - 1).
    /// C++: `getMaxIndex()`.
    pub fn get_max_index(&self) -> usize {
        if self.num_pieces == 0 {
            0
        } else {
            self.num_pieces - 1
        }
    }

    // ── Private trailing-bit cleanup helpers ──────────────────────────────

    /// Clears trailing bits beyond num_pieces in the completion bitfield.
    pub(super) fn clear_trailing_bits(&mut self) {
        if !self.num_pieces.is_multiple_of(8) {
            let extra = 8 - (self.num_pieces % 8);
            if let Some(last) = self.bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Clears trailing bits beyond num_pieces in the use bitfield.
    pub(super) fn clear_trailing_use_bits(&mut self) {
        if !self.num_pieces.is_multiple_of(8) {
            let extra = 8 - (self.num_pieces % 8);
            if let Some(last) = self.use_bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }
}
