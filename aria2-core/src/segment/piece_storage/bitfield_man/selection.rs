//! Stream piece selection algorithms for BitfieldMan.
//!
//! These methods implement the piece selection strategies for HTTP/FTP
//! (stream) downloads. They mirror the C++ BitfieldMan methods called
//! by the StreamPieceSelector hierarchy.
//!
//! The common pattern for filter-aware computation:
//!   - When filterEnabled_, a piece is "unavailable" if either:
//!     its filter bit is NOT set (not selected for download), OR
//!     its completion bit IS set (already downloaded), OR
//!     its use bit IS set (being downloaded), OR
//!     the ignore bitfield has it SET.
//!   - The "combined bitfield" (marking unavailable pieces) is computed as:
//!     ignore | ~filter | completion | use   (when filter enabled)
//!     ignore | completion | use            (when filter disabled)
//!
//! A set bit in the combined bitfield means "piece is NOT available for
//! selection" — we search for CLEAR bits in it.

use super::core::BitfieldMan;
use crate::segment::bitfield_util::{set_bit, test_bit};

impl BitfieldMan {
    /// Returns the first missing piece index.
    ///
    /// C++: `getFirstMissingIndex(index)` — finds the first piece that is
    /// not completed. When filter is enabled, only considers pieces with
    /// their filter bit set.
    pub fn get_first_missing_index(&self) -> Option<usize> {
        (0..self.num_pieces).find(|&i| {
            !test_bit(&self.bitfield, self.num_pieces, i)
                && (!self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i))
        })
    }

    /// Finds the first missing unused piece index starting from `start_index`,
    /// with `min_split_size` constraint.
    ///
    /// C++: `getInorderMissingUnusedIndex(index, startIndex, lastIndex, minSplitSize, ignoreBitfield, ignoreBitfieldLength)`
    ///
    /// The algorithm:
    /// 1. Always return `startIndex` if it is available (not in combined, not in use)
    /// 2. For subsequent indices, only return a piece if:
    ///    - The previous piece is not in-use and is "unavailable" in combined (completed/filtered/ignored — adjacent to data we won't conflict with), OR
    ///    - There are enough consecutive available pieces to satisfy `min_split_size`
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
            // whose start-1 piece is "unavailable" in the combined bitfield
            // (completed, ignored, or filter-excluded) and not in-use.
            // C++ checks `bitfield::test(bitfield, blocks, maxRange.startIndex-1)`
            // where `bitfield` is the COMBINED unavailable bitfield.
            let is_better = current_size > max_size
                || (current_size == max_size
                    && adjusted_start > 0
                    && max_range_start > 0
                    // max_range_start-1 NOT completed/filtered/ignored -> bad
                    && !test_bit(&combined, self.num_pieces, max_range_start - 1)
                    // adjusted_start-1 IS completed/filtered/ignored -> good
                    && test_bit(&combined, self.num_pieces, adjusted_start - 1));

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
            let range_end = std::cmp::min(self.num_pieces, (end as usize) + offset_index);
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
        let num_bytes = self.num_pieces.div_ceil(8);
        let mut combined = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            let ignored = test_bit(ignore_bitfield, self.num_pieces, i);
            let completed = test_bit(&self.bitfield, self.num_pieces, i);
            let in_use = test_bit(&self.use_bitfield, self.num_pieces, i);
            let filter_excluded =
                self.filter_enabled && !test_bit(&self.filter_bitfield, self.num_pieces, i);

            if ignored || completed || filter_excluded || in_use {
                set_bit(&mut combined, self.num_pieces, i);
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
}
