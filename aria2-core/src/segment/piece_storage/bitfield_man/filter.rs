//! Filter-related methods for BitfieldMan.
//!
//! Manages the filter bitfield which controls selective downloading.
//! Filter bits mark pieces that are INCLUDED in the download (bit set = included).

use super::core::BitfieldMan;
use super::helpers::{bf_count_set, bf_set, bf_unset};
use crate::segment::bitfield_util::test_bit;

impl BitfieldMan {
    // ── Filter enable/disable ─────────────────────────────────────────────

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

    /// Disables the filter. Filter bits are preserved but not used.
    /// C++: `disableFilter()` — sets `filterEnabled_ = false`.
    pub fn disable_filter(&mut self) {
        self.filter_enabled = false;
    }

    /// Clears and disables the filter bitfield entirely.
    /// C++: `clearFilter()` — deletes `filterBitfield_` and sets `filterEnabled_ = false`.
    pub fn clear_filter(&mut self) {
        self.filter_bitfield.fill(0);
        self.filter_enabled = false;
    }

    // ── Filter add/remove operations ──────────────────────────────────────

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
        let start_index = std::cmp::min((offset / self.piece_length) as usize, self.num_pieces);
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

    // ── Filter bit queries ────────────────────────────────────────────────

    /// Returns true if the filter bit is set for the given index.
    /// C++: `isFilterBitSet(index)` — returns false if filterBitfield_ is null.
    pub fn is_filter_bit_set(&self, index: usize) -> bool {
        if !self.filter_enabled || index >= self.num_pieces {
            return false;
        }
        test_bit(&self.filter_bitfield, self.num_pieces, index)
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

    /// Returns whether the filter is enabled.
    /// C++: `isFilterEnabled()`
    pub fn is_filter_enabled(&self) -> bool {
        self.filter_enabled
    }

    // ── Filter bitfield access ────────────────────────────────────────────

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

    /// Returns the bitfield byte length.
    pub fn bitfield_length(&self) -> usize {
        self.bitfield.len()
    }

    /// Clears trailing bits beyond num_pieces in the filter bitfield.
    /// Public so that SegmentMan can use it after manually modifying filter bits.
    pub fn clear_trailing_filter_bits(&mut self) {
        if !self.num_pieces.is_multiple_of(8) {
            let extra = 8 - (self.num_pieces % 8);
            if let Some(last) = self.filter_bitfield.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    // ── Filtered length calculations ──────────────────────────────────────

    /// Returns the filtered total length in bytes.
    ///
    /// C++: `getFilteredTotalLengthNow()` — computes the total length of
    /// all pieces that have their filter bit set.
    ///
    /// When filter is not enabled, C++ returns 0 (filterBitfield_ is null).
    /// This differs from `getFilteredCompletedLength()` which falls back to
    /// the unfiltered completed length when filter is disabled.
    pub fn get_filtered_total_length(&self) -> u64 {
        if !self.filter_enabled {
            // C++: if(!filterBitfield_) return 0;
            return 0;
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
        if self.num_pieces == 0 {
            return 0;
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
}
