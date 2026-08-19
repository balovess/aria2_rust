//! Peer-aware and range/offset query methods for BitfieldMan.
//!
//! Contains methods that query piece state against peer bitfields,
//! byte-offset ranges, and missing/unused piece collections.

use super::core::BitfieldMan;
use crate::segment::bitfield_util::{set_bit, test_bit};

impl BitfieldMan {
    // ── Peer-aware missing piece queries ──────────────────────────────────

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
    ///   This includes in-use pieces (for endgame mode).
    ///
    /// When filter is enabled, only pieces with filter bit set are considered.
    pub fn all_missing_indexes(&self, peer_bitfield: &[u8]) -> Vec<u8> {
        let num_bytes = self.num_pieces.div_ceil(8);
        let mut result = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && test_bit(peer_bitfield, self.num_pieces, i)
                && (!self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i))
            {
                set_bit(&mut result, self.num_pieces, i);
            }
        }

        result
    }

    /// Get a bitfield of all missing pieces (no peer constraint).
    ///
    /// C++ overload: `getAllMissingIndexes(misbitfield, mislen)` without
    /// a peer bitfield — returns all pieces we still need.
    /// When filter is enabled, only pieces with filter bit set are considered.
    pub fn all_missing_indexes_no_peer(&self) -> Vec<u8> {
        let num_bytes = self.num_pieces.div_ceil(8);
        let mut result = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && (!self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i))
            {
                set_bit(&mut result, self.num_pieces, i);
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
        let num_bytes = self.num_pieces.div_ceil(8);
        let mut result = vec![0u8; num_bytes];

        for i in 0..self.num_pieces {
            if !test_bit(&self.bitfield, self.num_pieces, i)
                && !test_bit(&self.use_bitfield, self.num_pieces, i)
                && test_bit(peer_bitfield, self.num_pieces, i)
                && (!self.filter_enabled || test_bit(&self.filter_bitfield, self.num_pieces, i))
            {
                set_bit(&mut result, self.num_pieces, i);
            }
        }

        result
    }

    // ── Range-based completion queries ────────────────────────────────────

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
    ///
    /// C++ returns false for zero/negative length or offset beyond total length.
    /// It also clamps `offset + length` to `totalLength`.
    pub fn is_bit_set_offset_range(&self, offset: u64, length: u64) -> bool {
        // C++: if(length <= 0 || totalLength_ <= offset) return false;
        if length == 0
            || offset >= self.total_length
            || self.num_pieces == 0
            || self.piece_length == 0
        {
            return false;
        }
        // C++: if(totalLength_ < offset + length) length = totalLength_ - offset;
        let effective_length = if offset + length > self.total_length {
            self.total_length - offset
        } else {
            length
        };
        let start_index = (offset / self.piece_length) as usize;
        let end_index = ((offset + effective_length - 1) / self.piece_length) as usize;
        // C++ is inclusive-inclusive; Rust is_bit_range_set is inclusive-exclusive
        self.is_bit_range_set(start_index, end_index + 1)
    }

    /// Get the completed length in bytes for pieces covering the byte range
    /// `[offset, offset+length)`.
    ///
    /// C++: `BitfieldMan::getOffsetCompletedLength(int64_t offset, int64_t length)`.
    /// Used for partial progress reporting.
    ///
    /// C++ clamps `offset + length` to `totalLength` before computing indices.
    pub fn get_offset_completed_length(&self, offset: u64, length: u64) -> u64 {
        if length == 0 || self.num_pieces == 0 {
            return 0;
        }
        // C++: if(totalLength_ < offset + length) length = totalLength_ - offset;
        let effective_length = if offset + length > self.total_length {
            self.total_length.saturating_sub(offset)
        } else {
            length
        };
        if effective_length == 0 {
            return 0;
        }
        let start_index = (offset / self.piece_length) as usize;
        let end_index = ((offset + effective_length - 1) / self.piece_length) as usize;
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
                let range_end = offset + effective_length;
                let overlap_start = piece_start.max(range_start);
                let overlap_end = piece_end.min(range_end);
                if overlap_end > overlap_start {
                    completed += overlap_end - overlap_start;
                }
            }
        }
        completed
    }

    // ── Missing/unused length and index queries ───────────────────────────

    /// Get the number of bytes of missing+unused pieces starting from a
    /// given piece index.
    ///
    /// C++: `BitfieldMan::getMissingUnusedLength(size_t startingIndex)`.
    /// Used to calculate how much data is available for download from
    /// a particular position.
    ///
    /// C++ stops at the first completed or in-use piece (returns contiguous
    /// missing+unused run only). Rust previously continued past gaps, which
    /// was incorrect.
    pub fn get_missing_unused_length(&self, starting_index: usize) -> u64 {
        if starting_index >= self.num_pieces {
            return 0;
        }
        let mut length: u64 = 0;
        for i in starting_index..self.num_pieces {
            // C++: if(isBitSet(i) || isUseBitSet(i)) break;
            if test_bit(&self.bitfield, self.num_pieces, i)
                || test_bit(&self.use_bitfield, self.num_pieces, i)
            {
                break;
            }
            length += self.get_block_length(i);
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
}
