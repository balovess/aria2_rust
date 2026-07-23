//! BitfieldMan — Manages piece-level bitfields.
//!
//! This is the Rust equivalent of the C++ `BitfieldMan` class.
//! It tracks three bitfields:
//! - **completion**: which pieces have been fully downloaded
//! - **use**: which pieces are currently being downloaded (in-flight)
//! - **filter**: which pieces are filtered out (not to be downloaded)

use super::super::bitfield_util::test_bit;

// ===========================================================================
// Bit manipulation helpers (MSB-first ordering, matching C++ aria2)
// ===========================================================================

/// Set bit at `index` in `bitfield` (MSB-first: bit 0 is the MSB of byte 0).
#[inline]
pub(crate) fn bf_set(bitfield: &mut [u8], index: usize) {
    let byte = index / 8;
    let bit = 7 - (index % 8);
    if byte < bitfield.len() {
        bitfield[byte] |= 1 << bit;
    }
}

/// Clear bit at `index` in `bitfield` (MSB-first).
#[inline]
pub(crate) fn bf_unset(bitfield: &mut [u8], index: usize) {
    let byte = index / 8;
    let bit = 7 - (index % 8);
    if byte < bitfield.len() {
        bitfield[byte] &= !(1 << bit);
    }
}

/// Count set bits in a bitfield up to `num_bits` bits.
pub(crate) fn bf_count_set(bitfield: &[u8], num_bits: usize) -> usize {
    if num_bits == 0 {
        return 0;
    }
    let full_bytes = num_bits / 8;
    let remaining_bits = num_bits % 8;
    let mut count: usize = bitfield[..full_bytes]
        .iter()
        .map(|b| b.count_ones() as usize)
        .sum();
    if remaining_bits > 0 && full_bytes < bitfield.len() {
        let last_byte = bitfield[full_bytes];
        let mask = !((1u8 << (8 - remaining_bits)) - 1);
        count += (last_byte & mask).count_ones() as usize;
    }
    count
}

// ===========================================================================
// BitfieldMan
// ===========================================================================

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
    bitfield: Vec<u8>,
    /// Bitfield tracking in-use pieces
    use_bitfield: Vec<u8>,
    /// Bitfield tracking filtered pieces
    filter_bitfield: Vec<u8>,
    /// Number of pieces
    num_pieces: usize,
    /// Length of each piece in bytes
    piece_length: u64,
    /// Total download length in bytes
    total_length: u64,
    /// Cached count of completed pieces
    cached_num_piece: usize,
    /// Whether the filter bitfield is enabled
    filter_enabled: bool,
}

impl BitfieldMan {
    /// Creates a new BitfieldMan with the given piece length and total length.
    pub fn new(piece_length: u64, total_length: u64) -> Self {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            ((total_length + piece_length - 1) / piece_length) as usize
        };
        let num_bytes = (num_pieces + 7) / 8;

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

    /// Returns true if there are missing pieces that are not in-use.
    pub fn has_missing_piece(&self) -> bool {
        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i) && !test_bit(&self.use_bitfield, self.num_pieces, i) {
                return true;
            }
        }
        false
    }

    /// Returns the index of the first missing unused piece.
    pub fn get_missing_piece_index(&self) -> Option<usize> {
        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i) && !test_bit(&self.use_bitfield, self.num_pieces, i) {
                return Some(i);
            }
        }
        None
    }

    /// Returns the index of the first missing unused piece that is not
    /// excluded by the ignore bitfield.
    ///
    /// A piece is "ignored" if the corresponding bit is SET in `ignore_bitfield`.
    /// Pieces whose bit is set in the ignore bitfield are skipped.
    pub fn get_missing_piece_index_with_ignore(
        &self,
        ignore_bitfield: &[u8],
    ) -> Option<usize> {
        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
                && !test_bit(ignore_bitfield, self.num_pieces, i)
            {
                return Some(i);
            }
        }
        None
    }

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

    /// Returns a reference to the completion bitfield.
    pub fn bitfield(&self) -> &[u8] {
        &self.bitfield
    }

    /// Sets the completion bitfield from external data.
    pub fn set_bitfield(&mut self, bitfield: &[u8]) {
        let copy_len = std::cmp::min(bitfield.len(), self.bitfield.len());
        self.bitfield[..copy_len].copy_from_slice(&bitfield[..copy_len]);
        self.cached_num_piece = bf_count_set(&self.bitfield, self.num_pieces);
    }

    /// Returns the number of remaining (not completed) pieces.
    pub fn count_missing_pieces(&self) -> usize {
        self.num_pieces.saturating_sub(self.cached_num_piece)
    }

    // ── Filter methods (used by SegmentMan) ─────────────────────────────

    /// Enables the filter bitfield.
    ///
    /// C++ `enableFilter()` only sets `filterEnabled_ = true` and calls
    /// `updateCache()`. It does NOT set all filter bits to 1. The filter
    /// bits are managed separately by `addFilter()` / `addNotFilter()`.
    /// The filter bitfield is initialized to all-zeros by `ensureFilterBitfield()`.
    ///
    /// In C++ aria2, `setupFileFilter()` calls `addFilter()` for each
    /// selected file range, THEN calls `enableFilter()`. The filter bits
    /// represent which pieces are INCLUDED in the download (bit set = included).
    /// An empty filter bitfield with filterEnabled means "nothing to download".
    pub fn enable_filter(&mut self) {
        self.filter_enabled = true;
    }

    /// Adds a filter that includes pieces covering the given byte range.
    ///
    /// In C++ aria2, `addFilter(offset, length)` sets the filter bit for pieces
    /// in `[offset, offset+length)`, marking them as **included** in the selective
    /// download. A filter bit set = piece is selected for download.
    ///
    /// The typical C++ flow is:
    /// 1. Call `addFilter()` for each requested file's byte range
    /// 2. Call `enableFilter()` to activate the filter
    ///
    /// C++ uses `endBlock = (offset + length - 1) / blockLength_` (inclusive).
    pub fn add_filter(&mut self, offset: u64, length: u64) {
        if self.num_pieces == 0 || length == 0 {
            return;
        }
        let start_index = (offset / self.piece_length) as usize;
        let end_index = std::cmp::min(
            ((offset + length - 1) / self.piece_length) as usize,
            self.num_pieces - 1,
        );
        for i in start_index..=end_index {
            bf_set(&mut self.filter_bitfield, i);
        }
        self.clear_trailing_filter_bits();
    }

    /// Removes a filter for pieces covering the given byte range.
    ///
    /// Clears filter bits for pieces in `[offset, offset+length)`,
    /// making those pieces not selected for download.
    pub fn remove_filter(&mut self, offset: u64, length: u64) {
        if self.num_pieces == 0 || length == 0 {
            return;
        }
        let start_index = (offset / self.piece_length) as usize;
        let end_index = std::cmp::min(
            ((offset + length - 1) / self.piece_length) as usize,
            self.num_pieces - 1,
        );
        for i in start_index..=end_index {
            bf_unset(&mut self.filter_bitfield, i);
        }
        self.clear_trailing_filter_bits();
    }

    /// Adds a NOT filter: marks pieces NOT in the given range as included.
    ///
    /// This is the C++ `addNotFilter` equivalent. Pieces OUTSIDE the range
    /// have their filter bits SET (included for download), while pieces INSIDE
    /// the range have their filter bits CLEARED (not selected).
    ///
    /// In C++: `addNotFilter(offset, length)` sets filter bits for
    /// `[0, startBlock)` and `[endBlock+1, blocks_)`, effectively
    /// selecting everything EXCEPT the specified range.
    pub fn add_not_filter(&mut self, offset: u64, length: u64) {
        if self.num_pieces == 0 || length == 0 {
            return;
        }
        let start_index = std::cmp::min(
            (offset / self.piece_length) as usize,
            self.num_pieces,
        );
        let end_index = std::cmp::min(
            ((offset + length - 1) / self.piece_length) as usize,
            self.num_pieces - 1,
        );
        // Set filter bits for pieces BEFORE the range
        for i in 0..start_index {
            bf_set(&mut self.filter_bitfield, i);
        }
        // Set filter bits for pieces AFTER the range
        for i in (end_index + 1)..self.num_pieces {
            bf_set(&mut self.filter_bitfield, i);
        }
        self.clear_trailing_filter_bits();
    }

    /// Returns true if all filter bits are set (all pieces are selected for download).
    ///
    /// C++: `isAllFilterBitSet()` — checks if every piece has its filter bit set.
    pub fn is_all_filter_bit_set(&self) -> bool {
        if self.num_pieces == 0 {
            return false;
        }
        bf_count_set(&self.filter_bitfield, self.num_pieces) == self.num_pieces
    }

    /// Returns the filter bitfield as a byte slice.
    pub fn get_filter_bitfield(&self) -> &[u8] {
        &self.filter_bitfield
    }

    /// Returns the filter bitfield as a mutable byte slice.
    pub fn get_filter_bitfield_mut(&mut self) -> &mut [u8] {
        &mut self.filter_bitfield
    }

    /// Returns the byte length of the bitfield storage.
    pub fn get_bitfield_length(&self) -> usize {
        self.bitfield.len()
    }

    /// Clears trailing bits beyond num_pieces in the filter bitfield.
    /// Public so that SegmentMan can use it after manually modifying filter bits.
    pub fn clear_trailing_filter_bits(&mut self) {
        if self.num_pieces % 8 != 0 {
            let extra = 8 - (self.num_pieces % 8);
            if let Some(last) = self.filter_bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Returns the bitfield byte length.
    pub fn bitfield_length(&self) -> usize {
        self.bitfield.len()
    }

    /// Check if there are pieces we need that the peer has.
    ///
    /// Mirrors C++ `BitfieldMan::hasMissingPiece(peerBitfield, peerBitfieldLength)`.
    /// Returns true if any piece is:
    /// - NOT in our completed bitfield (we need it)
    /// - IS in the peer's bitfield (peer has it)
    /// - (if filter enabled) Has its filter bit set (selected for download)
    pub fn has_missing_piece_with_bitfield(&self, peer_bitfield: &[u8]) -> bool {
        for i in 0..self.num_pieces {
            // We need this piece (not completed)
            if !test_bit(&self.bitfield, self.num_pieces, i) {
                // Peer has this piece
                if test_bit(peer_bitfield, self.num_pieces, i) {
                    // If filter enabled, only consider filtered pieces
                    if !self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Get a bitfield of all pieces that are missing and the peer has.
    ///
    /// Mirrors C++ `BitfieldMan::getAllMissingIndexes()`.
    /// Returns a bitfield where set bits represent pieces that:
    /// - Are NOT in our completed bitfield (we need them)
    /// - ARE in the peer's bitfield (peer has them)
    /// This includes in-use pieces (for endgame mode).
    ///
    /// When filter is enabled, only pieces with filter bit set are considered.
    pub fn all_missing_indexes(&self, peer_bitfield: &[u8]) -> Vec<u8> {
        let num_bytes = (self.num_pieces + 7) / 8;
        let mut result = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && test_bit(peer_bitfield, self.num_pieces, i)
                && (!self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i))
            {
                super::super::bitfield_util::set_bit(&mut result, self.num_pieces, i);
            }
        }

        result
    }

    /// Get a bitfield of all pieces that are missing, unused, and the peer has.
    ///
    /// Mirrors C++ `BitfieldMan::getAllMissingUnusedIndexes()`.
    /// Returns a bitfield where set bits represent pieces that:
    /// - Are NOT in our completed bitfield (we need them)
    /// - Are NOT in our use bitfield (not being downloaded)
    /// - ARE in the peer's bitfield (peer has them)
    ///
    /// When filter is enabled, only pieces with filter bit set are considered.
    pub fn all_missing_unused_indexes(&self, peer_bitfield: &[u8]) -> Vec<u8> {
        let num_bytes = (self.num_pieces + 7) / 8;
        let mut result = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
                && test_bit(peer_bitfield, self.num_pieces, i)
                && (!self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i))
            {
                super::super::bitfield_util::set_bit(&mut result, self.num_pieces, i);
            }
        }

        result
    }

    // ── Additional BitfieldMan methods matching C++ ──────────────────────

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

    /// Disables the filter. Filter bits are preserved but not used.
    /// C++: `disableFilter()` — sets `filterEnabled_ = false`.
    pub fn disable_filter(&mut self) {
        self.filter_enabled = false;
    }

    /// Clears and disables the filter bitfield entirely.
    /// C++: `clearFilter()` — deletes `filterBitfield_` and sets `filterEnabled_ = false`.
    pub fn clear_filter(&mut self) {
        for byte in &mut self.filter_bitfield {
            *byte = 0;
        }
        self.filter_enabled = false;
    }

    /// Returns true if the filter bit is set for the given index.
    /// C++: `isFilterBitSet(index)` — returns false if filterBitfield_ is null.
    pub fn is_filter_bit_set(&self, index: usize) -> bool {
        if !self.filter_enabled || index >= self.num_pieces {
            return false;
        }
        test_bit(&self.filter_bitfield, self.num_pieces, index)
    }

    /// Returns true if all filtered pieces are completed.
    ///
    /// C++: `isFilteredAllBitSet()` — checks that every piece with its
    /// filter bit set also has its completion bit set. This is used by
    /// `downloadFinished()` to determine if the selective download is complete.
    pub fn is_filtered_all_bit_set(&self) -> bool {
        if !self.filter_enabled {
            // No filter active: all completed pieces means download finished
            return self.cached_num_piece == self.num_pieces;
        }
        // Every piece with filter bit set must have completion bit set
        for i in 0..self.num_pieces {
            if test_bit(&self.filter_bitfield, self.num_pieces, i)
                && !test_bit(&self.bitfield, self.num_pieces, i)
            {
                return false;
            }
        }
        true
    }

    /// Sets bits in the completion bitfield for the range [start, end].
    /// C++: `setBitRange(startIndex, endIndex)` — inclusive on both ends.
    pub fn set_bit_range(&mut self, start_index: usize, end_index: usize) {
        let end = std::cmp::min(end_index, self.num_pieces - 1);
        for i in start_index..=end {
            self.set_piece(i);
        }
    }

    /// Clears bits in the completion bitfield for the range [start, end].
    /// C++: `unsetBitRange(startIndex, endIndex)` — inclusive on both ends.
    pub fn unset_bit_range(&mut self, start_index: usize, end_index: usize) {
        let end = std::cmp::min(end_index, self.num_pieces - 1);
        for i in start_index..=end {
            self.clear_piece(i);
        }
    }

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

    /// Returns the filtered total length in bytes.
    ///
    /// C++: `getFilteredTotalLengthNow()` — computes the total length of
    /// all pieces that have their filter bit set.
    pub fn get_filtered_total_length(&self) -> u64 {
        if !self.filter_enabled {
            return self.total_length;
        }
        let filtered_count = bf_count_set(&self.filter_bitfield, self.num_pieces);
        if filtered_count == 0 {
            return 0;
        }
        // If the last piece has its filter bit set, it may be shorter
        let last_index = self.num_pieces - 1;
        if test_bit(&self.filter_bitfield, self.num_pieces, last_index) {
            (filtered_count - 1) as u64 * self.piece_length + self.get_last_block_length()
        } else {
            filtered_count as u64 * self.piece_length
        }
    }

    /// Returns the filtered completed length in bytes.
    ///
    /// C++: `getFilteredCompletedLengthNow()` — computes the completed length
    /// of pieces that have their filter bit set.
    pub fn get_filtered_completed_length(&self) -> u64 {
        if !self.filter_enabled {
            return self.get_completed_length();
        }
        let mut completed_length: u64 = 0;
        let last_index = self.num_pieces - 1;
        for i in 0..self.num_pieces {
            if test_bit(&self.filter_bitfield, self.num_pieces, i)
                && test_bit(&self.bitfield, self.num_pieces, i)
            {
                if i == last_index {
                    completed_length += self.get_last_block_length();
                } else {
                    completed_length += self.piece_length;
                }
            }
        }
        completed_length
    }

    /// Returns whether the filter is enabled.
    /// C++: `isFilterEnabled()`
    pub fn is_filter_enabled(&self) -> bool {
        self.filter_enabled
    }

    // ── Range-based queries (C++ BitfieldMan methods) ─────────────────────

    /// Check whether all pieces in the range `[start_index, end_index)` have
    /// their completion bit set.
    ///
    /// C++: `BitfieldMan::isBitRangeSet(size_t startIndex, size_t endIndex)`.
    /// Used for partial segment integrity verification.
    pub fn is_bit_range_set(&self, start_index: usize, end_index: usize) -> bool {
        for i in start_index..end_index {
            if !test_bit(&self.bitfield, self.num_pieces, i) {
                return false;
            }
        }
        true
    }

    /// Check whether all pieces covering the byte range `[offset, offset+length)`
    /// have their completion bit set.
    ///
    /// C++: `BitfieldMan::isBitSetOffsetRange(int64_t offset, int64_t length)`.
    /// Used to verify that a byte range is fully available before reading.
    pub fn is_bit_set_offset_range(&self, offset: u64, length: u64) -> bool {
        if length == 0 {
            return true;
        }
        let start_index = (offset / self.piece_length) as usize;
        let end_index = ((offset + length - 1) / self.piece_length) as usize;
        self.is_bit_range_set(start_index, end_index + 1)
    }

    /// Get the completed length in bytes for pieces covering the byte range
    /// `[offset, offset+length)`.
    ///
    /// C++: `BitfieldMan::getOffsetCompletedLength(int64_t offset, int64_t length)`.
    /// Used for partial progress reporting.
    pub fn get_offset_completed_length(&self, offset: u64, length: u64) -> u64 {
        if length == 0 || self.num_pieces == 0 {
            return 0;
        }
        let start_index = (offset / self.piece_length) as usize;
        let end_index = ((offset + length - 1) / self.piece_length) as usize;
        let mut completed: u64 = 0;
        for i in start_index..=end_index {
            if i >= self.num_pieces {
                break;
            }
            if test_bit(&self.bitfield, self.num_pieces, i) {
                // Calculate how many bytes of this piece fall within the range
                let piece_start = i as u64 * self.piece_length;
                let piece_end = piece_start + self.get_block_length(i);
                let range_start = offset;
                let range_end = offset + length;
                let overlap_start = piece_start.max(range_start);
                let overlap_end = piece_end.min(range_end);
                if overlap_end > overlap_start {
                    completed += overlap_end - overlap_start;
                }
            }
        }
        completed
    }

    /// Get the number of bytes of missing+unused pieces starting from a
    /// given piece index.
    ///
    /// C++: `BitfieldMan::getMissingUnusedLength(size_t startingIndex)`.
    /// Used to calculate how much data is available for download from
    /// a particular position.
    pub fn get_missing_unused_length(&self, starting_index: usize) -> u64 {
        let mut length: u64 = 0;
        for i in starting_index..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
            {
                length += self.get_block_length(i);
            }
        }
        length
    }

    /// Get the first N missing+unused piece indexes.
    ///
    /// C++: `BitfieldMan::getFirstNMissingUnusedIndex(vector<size_t>&, size_t n)`.
    /// Used for DHT/PEX reporting and batch request generation.
    pub fn get_first_n_missing_unused_indexes(&self, n: usize) -> Vec<usize> {
        let mut result = Vec::with_capacity(n);
        for i in 0..self.num_pieces {
            if result.len() >= n {
                break;
            }
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
            {
                result.push(i);
            }
        }
        result
    }

    // ── Stream piece selection algorithms ──────────────────────────────────
    //
    // These methods implement the piece selection strategies for HTTP/FTP
    // (stream) downloads. They mirror the C++ BitfieldMan methods called
    // by the StreamPieceSelector hierarchy.
    //
    // The common pattern for filter-aware computation:
    //   - When filterEnabled_, a piece is "unavailable" if either:
    //     its filter bit is NOT set (not selected for download), OR
    //     its completion bit IS set (already downloaded), OR
    //     its use bit IS set (being downloaded), OR
    //     the ignore bitfield has it SET.
    //   - The "combined bitfield" (marking unavailable pieces) is computed as:
    //     ignore | ~filter | completion | use   (when filter enabled)
    //     ignore | completion | use            (when filter disabled)
    //
    // A set bit in the combined bitfield means "piece is NOT available for
    // selection" — we search for CLEAR bits in it.

    /// Returns the first missing piece index.
    ///
    /// C++: `getFirstMissingIndex(index)` — finds the first piece that is
    /// not completed. When filter is enabled, only considers pieces with
    /// their filter bit set.
    pub fn get_first_missing_index(&self) -> Option<usize> {
        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i) {
                if !self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i) {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Finds the first missing unused piece index starting from `start_index`,
    /// with `min_split_size` constraint.
    ///
    /// C++: `getInorderMissingUnusedIndex(index, startIndex, lastIndex, minSplitSize, ignoreBitfield, ignoreBitfieldLength)`
    ///
    /// The algorithm:
    /// 1. Always return `startIndex` if it is available (not in combined, not in use)
    /// 2. For subsequent indices, only return a piece if:
    ///    a. The previous piece is not in-use and is "unavailable" in combined
    ///       (meaning completed/filtered/ignored — adjacent to data we won't
    ///       conflict with), OR
    ///    b. There are enough consecutive available pieces to satisfy
    ///       `min_split_size`
    pub fn get_inorder_missing_unused_index(
        &self,
        start_index: usize,
        end_index: usize,
        min_split_size: u64,
        ignore_bitfield: &[u8],
    ) -> Option<usize> {
        if self.num_pieces == 0 {
            return None;
        }
        let end = std::cmp::min(end_index, self.num_pieces);

        // Build the combined "unavailable" bitfield.
        // A set bit means: piece is NOT available for selection.
        let combined = self.build_combined_unavailable(ignore_bitfield);

        // Priority: always return start_index if it's clear in combined
        // (which already includes use_bitfield)
        if start_index < end && !test_bit(&combined, self.num_pieces, start_index) {
            return Some(start_index);
        }

        let mut i = start_index + 1;
        while i < end {
            if !test_bit(&combined, self.num_pieces, i) {
                // Check if previous piece is not in-use and is "unavailable"
                // (completed/filtered/ignored — we can safely start here)
                // C++: !test(useBitfield, blocks, i-1) && test(bitfield=combined, blocks, i-1)
                if i > 0
                    && !test_bit(&self.use_bitfield, self.num_pieces, i - 1)
                    && test_bit(&combined, self.num_pieces, i - 1)
                {
                    return Some(i);
                }
                // Check for min_split_size consecutive free space
                // C++ iterates through ALL blocks (not just up to lastIndex)
                let mut j = i;
                while j < self.num_pieces {
                    if test_bit(&combined, self.num_pieces, j) {
                        break;
                    }
                    if (j - i + 1) as u64 * self.piece_length >= min_split_size {
                        return Some(j);
                    }
                    j += 1;
                }
                i = j + 1;
            } else {
                i += 1;
            }
        }
        None
    }

    /// Finds the missing unused piece at the midpoint of the longest missing run.
    ///
    /// C++: `getSparseMissingUnusedIndex(index, minSplitSize, ignoreBitfield, ignoreBitfieldLength)`
    ///
    /// The sparse algorithm scans for contiguous "runs" of missing+unused pieces
    /// and selects the midpoint of the longest run. This maximizes parallelism
    /// by spreading connections across the file.
    ///
    /// If the start of a run is adjacent to a completed piece (previous piece
    /// is completed and not in-use), prefer to start from the beginning of
    /// that run. Otherwise, return the midpoint.
    pub fn get_sparse_missing_unused_index(
        &self,
        min_split_size: u64,
        ignore_bitfield: &[u8],
    ) -> Option<usize> {
        if self.num_pieces == 0 {
            return None;
        }

        let combined = self.build_combined_unavailable(ignore_bitfield);

        // Scan for contiguous runs of available (clear) bits
        let mut max_range_start = 0usize;
        let mut max_range_end = 0usize;
        let mut next_index = 0usize;

        while next_index < self.num_pieces {
            // Find start of a run: first index where combined bit is CLEAR
            let start = Self::find_start_index(next_index, &combined, self.num_pieces);
            if start >= self.num_pieces {
                break;
            }
            // Find end of the run: first index where combined bit is SET again
            let end = Self::find_end_index(start, &combined, self.num_pieces);

            let mut adjusted_start = start;
            // If the piece just before the run is in-use, start from midpoint
            // of the run instead (to avoid interfering with an active download)
            if start > 0 && test_bit(&self.use_bitfield, self.num_pieces, start - 1) {
                adjusted_start = (start + end) / 2;
            }

            let current_size = end - adjusted_start;
            let max_size = max_range_end.saturating_sub(max_range_start);

            // Prefer larger ranges. For equal-sized ranges, prefer the one
            // whose start-1 piece is completed and not in-use (adjacent to
            // completed data, so sequential download benefits).
            let is_better = current_size > max_size
                || (current_size == max_size
                    && adjusted_start > 0
                    && max_range_start > 0
                    && (!test_bit(&self.bitfield, self.num_pieces, max_range_start - 1)
                        || test_bit(&self.use_bitfield, self.num_pieces, max_range_start - 1))
                    && test_bit(&self.bitfield, self.num_pieces, adjusted_start - 1)
                    && !test_bit(&self.use_bitfield, self.num_pieces, adjusted_start - 1));

            if is_better {
                max_range_start = adjusted_start;
                max_range_end = end;
            }

            next_index = end;
        }

        if max_range_end > max_range_start {
            // If range starts at index 0, always return 0
            if max_range_start == 0 {
                return Some(0);
            }
            // Return start if previous piece is not in-use and is "unavailable"
            // (completed/filtered/ignored — safe to start here), OR the range
            // is large enough for min_split_size.
            // C++: (!test(useBitfield, blocks, maxRange.startIndex-1) &&
            //        test(combined, blocks, maxRange.startIndex-1)) ||
            //       (range_size * blockLength >= minSplitSize)
            if (!test_bit(&self.use_bitfield, self.num_pieces, max_range_start - 1)
                && test_bit(&combined, self.num_pieces, max_range_start - 1))
                || ((max_range_end - max_range_start) as u64 * self.piece_length >= min_split_size)
            {
                return Some(max_range_start);
            }
        }
        None
    }

    /// Finds a missing unused piece using geometric progression from an offset.
    ///
    /// C++: `getGeomMissingUnusedIndex(index, minSplitSize, ignoreBitfield, ignoreBitfieldLength, base, offsetIndex)`
    ///
    /// The geometric algorithm searches increasingly large windows starting
    /// from `offset_index`. Window sizes follow base^0, base^1, base^2, ...
    /// Within each window, it picks the first missing unused piece. If no
    /// piece is found in any window, it falls back to sparse selection.
    pub fn get_geom_missing_unused_index(
        &self,
        min_split_size: u64,
        ignore_bitfield: &[u8],
        base: f64,
        offset_index: usize,
    ) -> Option<usize> {
        if self.num_pieces == 0 {
            return None;
        }

        let combined = self.build_combined_unavailable(ignore_bitfield);

        let mut start: f64 = 0.0;
        let mut end: f64 = 1.0;

        while (start as usize) + offset_index < self.num_pieces {
            // Search within [start+offset, end+offset) for a missing unused piece
            let range_end = std::cmp::min(
                self.num_pieces,
                (end as usize) + offset_index,
            );
            let mut found_index = self.num_pieces; // sentinel: not found

            for i in (start as usize) + offset_index..range_end {
                if test_bit(&self.use_bitfield, self.num_pieces, i) {
                    // Piece is in-use, stop searching this window
                    break;
                } else if !test_bit(&combined, self.num_pieces, i) {
                    // Piece is missing and not in-use — select it
                    found_index = i;
                    break;
                }
            }

            if found_index < self.num_pieces {
                return Some(found_index);
            }

            // Expand window geometrically
            start = end;
            end *= base;
        }

        // Fallback: sparse selection
        self.get_sparse_missing_unused_index(min_split_size, ignore_bitfield)
    }

    // ── Private helpers for stream piece selection ─────────────────────────

    /// Build the combined "unavailable" bitfield from completion, filter,
    /// use, and ignore bitfields. A set bit means the piece is NOT available.
    ///
    /// C++ computes: `ignore | ~filter | completion | use` (when filter enabled)
    /// or `ignore | completion | use` (when filter disabled).
    fn build_combined_unavailable(&self, ignore_bitfield: &[u8]) -> Vec<u8> {
        let num_bytes = (self.num_pieces + 7) / 8;
        let mut combined = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            let ignored = test_bit(ignore_bitfield, self.num_pieces, i);
            let completed = test_bit(&self.bitfield, self.num_pieces, i);
            let in_use = test_bit(&self.use_bitfield, self.num_pieces, i);
            let filter_excluded =
                self.filter_enabled && !test_bit(&self.filter_bitfield, self.num_pieces, i);

            if ignored || completed || filter_excluded || in_use {
                super::super::bitfield_util::set_bit(&mut combined, self.num_pieces, i);
            }
        }
        combined
    }

    /// Find the start of the next run: first index >= `from` where the
    /// combined bitfield bit is CLEAR (piece is available for selection).
    /// C++: `getStartIndex(index, bitfield, blocks)`
    fn find_start_index(from: usize, combined: &[u8], num_pieces: usize) -> usize {
        let mut index = from;
        while index < num_pieces && test_bit(combined, num_pieces, index) {
            index += 1;
        }
        index
    }

    /// Find the end of the current run: first index >= `from` where the
    /// bitfield bit is SET again (piece is NOT available).
    fn find_end_index(from: usize, combined: &[u8], num_pieces: usize) -> usize {
        let mut index = from;
        while index < num_pieces && !test_bit(combined, num_pieces, index) {
            index += 1;
        }
        index
    }

    /// Clears trailing bits beyond num_pieces in the completion bitfield.
    fn clear_trailing_bits(&mut self) {
        if self.num_pieces % 8 != 0 {
            let extra = 8 - (self.num_pieces % 8);
            if let Some(last) = self.bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Clears trailing bits beyond num_pieces in the use bitfield.
    fn clear_trailing_use_bits(&mut self) {
        if self.num_pieces % 8 != 0 {
            let extra = 8 - (self.num_pieces % 8);
            if let Some(last) = self.use_bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }
}
