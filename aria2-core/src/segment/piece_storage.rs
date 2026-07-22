//! Piece storage management for segmented downloads.
//!
//! This module provides the [`PieceStorage`] trait and [`DefaultPieceStorage`]
//! implementation for tracking download progress at the piece level. It supports
//! both HTTP segmented downloads and BitTorrent piece-based downloads.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PieceStorage.h` — Piece storage interface
//! - `src/DefaultPieceStorage.h/.cc` — Default implementation
//! - `src/Piece.h` — Piece class
//! - `src/BitfieldMan.h` — Bitfield management
//!
//! # Key Types
//!
//! - [`BitfieldMan`] — Manages completion/usage/filter bitfields for piece tracking
//! - [`Piece`] — Represents a single downloadable piece with block-level tracking
//! - [`PieceStorage`] — Trait interface for piece storage operations
//! - [`DefaultPieceStorage`] — Default implementation suitable for HTTP/FTP and BT

use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, trace};

use super::bitfield_util::test_bit;
use super::piece::Piece;

#[cfg(feature = "bittorrent")]
use crate::engine::bt_peer_connection::BtPeerConn;
#[cfg(feature = "bittorrent")]
use crate::engine::bt_peer_interaction::PieceProvider;
#[cfg(feature = "bittorrent")]
use super::piece_selector::{PieceSelectorKind, RarestPieceSelector};
#[cfg(feature = "bittorrent")]
use super::piece_stat_man::PieceStatMan;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default number of remaining pieces that trigger end-game mode.
const END_GAME_PIECE_NUM: usize = 20;

// ===========================================================================
// Bit manipulation helpers (MSB-first ordering, matching C++ aria2)
// ===========================================================================

/// Set bit at `index` in `bitfield` (MSB-first: bit 0 is the MSB of byte 0).
#[inline]
fn bf_set(bitfield: &mut [u8], index: usize) {
    let byte = index / 8;
    let bit = 7 - (index % 8);
    if byte < bitfield.len() {
        bitfield[byte] |= 1 << bit;
    }
}

/// Clear bit at `index` in `bitfield` (MSB-first).
#[inline]
fn bf_unset(bitfield: &mut [u8], index: usize) {
    let byte = index / 8;
    let bit = 7 - (index % 8);
    if byte < bitfield.len() {
        bitfield[byte] &= !(1 << bit);
    }
}

/// Count set bits in a bitfield up to `num_bits` bits.
fn bf_count_set(bitfield: &[u8], num_bits: usize) -> usize {
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
// BitfieldMan — Manages piece-level bitfields
// ===========================================================================

/// Manages completion, usage, and filter bitfields for piece tracking.
///
/// This is the Rust equivalent of the C++ `BitfieldMan` class.
/// It tracks three bitfields:
/// - **completion**: which pieces have been fully downloaded
/// - **use**: which pieces are currently being downloaded (in-flight)
/// - **filter**: which pieces are filtered out (not to be downloaded)
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
                super::bitfield_util::set_bit(&mut result, self.num_pieces, i);
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
                super::bitfield_util::set_bit(&mut result, self.num_pieces, i);
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
                super::bitfield_util::set_bit(&mut combined, self.num_pieces, i);
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

// ===========================================================================
// StreamPieceSelectorKind — HTTP/FTP piece selection strategies
// ===========================================================================

/// Enum dispatch for stream (HTTP/FTP) piece selection strategies.
///
/// Replaces C++ `StreamPieceSelector` hierarchy:
/// - `DefaultStreamPieceSelector` → sparse mid-point selection
/// - `InorderStreamPieceSelector` → sequential from start
/// - `RandomStreamPieceSelector` → random starting point
/// - `GeomStreamPieceSelector` → geometric distribution
///
/// The default is `Default` (sparse), matching C++ behavior when
/// `PREF_STREAM_PIECE_SELECTOR` is empty or "default".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPieceSelectorKind {
    /// Default/sparse: selects the midpoint of the longest missing run.
    /// C++ `DefaultStreamPieceSelector`.
    Default,
    /// Sequential: selects the first missing piece from the beginning.
    /// C++ `InorderStreamPieceSelector`.
    Inorder,
    /// Random: starts at a random offset, then falls back to inorder.
    /// C++ `RandomStreamPieceSelector`.
    Random,
    /// Geometric: uses geometric progression from the last completed piece.
    /// C++ `GeomStreamPieceSelector` with base 1.5.
    Geom,
}

// ===========================================================================
// PieceStorage trait
// ===========================================================================

/// Trait interface for piece storage operations.
///
/// This is the Rust equivalent of the C++ `PieceStorage` abstract class.
/// Methods are aligned with the C++ interface; BT-specific peer-overloaded
/// methods live in the separate `PieceProvider` trait.
pub trait PieceStorage: Send + Sync {
    // ── Piece query ──────────────────────────────────────────────────────

    /// Returns true if there are missing pieces that are not in-use.
    fn has_missing_unused_piece(&self) -> bool;

    /// Gets the next missing piece to download.
    /// C++: `getMissingPiece(minSplitSize, ignoreBitfield, length, cuid)`
    fn get_missing_piece(
        &mut self,
        min_split_size: u64,
        ignore_bitfield: &[u8],
        length: u64,
        cuid: u64,
    ) -> Option<Piece>;

    /// Gets a specific missing piece by index.
    /// C++: `getMissingPiece(index, cuid)`
    fn get_missing_piece_by_index(&mut self, index: usize, cuid: u64) -> Option<Piece>;

    /// Returns the piece denoted by index without changing its status.
    /// C++: `getPiece(index)` — used for uploading (no checkout).
    fn get_piece(&self, index: usize) -> Option<Piece>;

    /// Marks a piece as completed.
    fn complete_piece(&mut self, piece: &Piece) -> bool;

    /// Cancels a piece download.
    fn cancel_piece(&mut self, piece: &mut Piece, cuid: u64);

    /// Returns true if the piece at the given index is completed.
    fn has_piece(&self, index: usize) -> bool;

    /// Returns true if the piece at the given index is in-use.
    fn is_piece_used(&self, index: usize) -> bool;

    // ── Length / progress ────────────────────────────────────────────────

    /// Returns the total download length in bytes.
    fn get_total_length(&self) -> u64;

    /// Returns the filtered total length in bytes.
    /// C++: `getFilteredTotalLength()`
    fn get_filtered_total_length(&self) -> u64;

    /// Returns the completed length in bytes.
    fn get_completed_length(&self) -> u64;

    /// Returns the filtered completed length in bytes.
    /// C++: `getFilteredCompletedLength()`
    fn get_filtered_completed_length(&self) -> u64;

    // ── Completion ───────────────────────────────────────────────────────

    /// Returns true if all pieces are downloaded (or filtered pieces if
    /// selective downloading is enabled).
    fn download_finished(&self) -> bool;

    /// Returns true if all downloads are finished (ignoring filter).
    fn all_download_finished(&self) -> bool;

    // ── Bitfield ─────────────────────────────────────────────────────────

    /// Returns the completion bitfield.
    fn get_bitfield(&self) -> Vec<u8>;

    /// Returns the bitfield length in bytes.
    /// C++: `getBitfieldLength()`
    fn get_bitfield_length(&self) -> usize;

    /// Sets the completion bitfield.
    fn set_bitfield(&mut self, bitfield: &[u8]);

    // ── Marking ──────────────────────────────────────────────────────────

    /// Marks all pieces as completed.
    /// C++: `markAllPiecesDone()`
    fn mark_all_pieces_done(&mut self);

    /// Marks pieces as done up to the given length.
    fn mark_pieces_done(&mut self, length: u64);

    /// Marks the piece at the given index as missing (incomplete).
    /// C++: `markPieceMissing(index)` — used after hash verification failure.
    fn mark_piece_missing(&mut self, index: usize);

    // ── End-game ─────────────────────────────────────────────────────────

    /// Returns true if in end-game mode.
    fn is_end_game(&self) -> bool;

    /// Enters end-game mode.
    fn enter_end_game(&mut self);

    /// Sets the number of remaining pieces that trigger end-game mode.
    /// C++: `setEndGamePieceNum(num)`
    fn set_end_game_piece_num(&mut self, num: usize);

    // ── Piece length ─────────────────────────────────────────────────────

    /// Returns the length of the piece at the given index.
    /// The last piece may be shorter than `piece_length`.
    /// C++: `getPieceLength(index)`
    fn get_piece_length(&self, index: usize) -> u32;

    // ── Have advertisement (used by BT interaction) ──────────────────────

    /// Advertise that a piece was completed by the given CUID.
    /// Other commands will send Have messages based on this.
    /// C++: `advertisePiece(cuid, index, registeredTime)`
    fn advertise_piece(&mut self, cuid: u64, index: usize);

    /// Get piece indexes advertised since `last_have_index` by CUIDs
    /// other than `my_cuid`. Returns the new `last_have_index`.
    /// C++: `getAdvertisedPieceIndexes(indexes, myCuid, lastHaveIndex)`
    fn get_advertised_piece_indexes(
        &self,
        my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64);

    /// Remove have entries older than `expiry` (in millis since epoch).
    /// C++: `removeAdvertisedPiece(expiry)`
    fn remove_advertised_piece(&mut self, expiry_ms: u64);

    // ── In-flight pieces (used by session resume) ────────────────────────

    /// Add pieces that are currently in-flight (being downloaded).
    /// C++: `addInFlightPiece(pieces)`
    fn add_in_flight_piece(&mut self, piece: Piece);

    /// Returns the number of in-flight pieces.
    /// C++: `countInFlightPiece()`
    fn count_in_flight_piece(&self) -> usize;

    /// Returns all in-flight pieces.
    /// C++: `getInFlightPieces(pieces)`
    fn get_in_flight_pieces(&self) -> Vec<Piece>;

    // ── Piece statistics (for rarest-first selection) ────────────────────

    /// Increment piece stat for the given index.
    /// C++: `addPieceStats(index)`
    fn add_piece_stats_for_index(&mut self, index: usize);

    /// Increment piece stats for each set bit in the bitfield.
    /// C++: `addPieceStats(bitfield, bitfieldLength)`
    fn add_piece_stats(&mut self, bitfield: &[u8]);

    /// Decrement piece stats for each set bit in the bitfield.
    /// C++: `subtractPieceStats(bitfield, bitfieldLength)`
    fn subtract_piece_stats(&mut self, bitfield: &[u8]);

    /// Update piece stats: add for new bits, subtract for removed bits.
    /// C++: `updatePieceStats(newBitfield, newBitfieldLength, oldBitfield)`
    fn update_piece_stats(&mut self, new_bitfield: &[u8], old_bitfield: &[u8]);

    // ── Navigation ───────────────────────────────────────────────────────

    /// Returns the next used index after `index`, or `num_pieces` if none.
    /// C++: `getNextUsedIndex(index)`
    fn get_next_used_index(&self, index: usize) -> usize;

    // ── Lifecycle ────────────────────────────────────────────────────────

    /// Called when the system detects the download is not finished.
    /// C++: `onDownloadIncomplete()`
    fn on_download_incomplete(&mut self);

    // ── Selective downloading ────────────────────────────────────────────

    /// Returns true if selective downloading mode is active.
    /// C++: `isSelectiveDownloadingMode()`
    fn is_selective_downloading_mode(&self) -> bool;
}

// ===========================================================================
// DefaultPieceStorage
// ===========================================================================

/// Entry tracking a "have" advertisement for a piece.
///
/// Mirrors the C++ `HaveEntry` struct. When a command completes a piece,
/// it advertises it. Other commands query the advertised list to send
/// Have messages to their peers.
struct HaveEntry {
    /// Monotonically increasing sequence number for ordering.
    have_index: u64,
    /// The CUID that completed the piece.
    cuid: u64,
    /// The piece index that was completed.
    index: usize,
    /// Time when this entry was registered (millis since epoch).
    registered_time_ms: u64,
}

/// Default implementation of PieceStorage for HTTP/FTP and BitTorrent downloads.
///
/// Uses `BitfieldMan` for piece tracking and supports piece selection strategies.
/// Mirrors C++ `DefaultPieceStorage`.
pub struct DefaultPieceStorage {
    /// Bitfield manager for piece tracking
    bfman: BitfieldMan,
    /// Pieces currently in-flight (index -> Piece)
    used_pieces: HashMap<usize, Piece>,
    /// Whether we are in end-game mode
    end_game: bool,
    /// Number of remaining pieces that trigger end-game mode
    end_game_piece_num: usize,
    /// Total length of the download
    total_length: u64,
    /// Piece length in bytes
    piece_length: u64,
    /// Monotonically increasing have-index for HaveEntry ordering
    next_have_index: u64,
    /// Queue of Have entries (advertised piece completions)
    haves: Vec<HaveEntry>,
    /// Piece statistics manager for rarest-first selection.
    /// Shared with PieceSelector via Arc.
    #[cfg(feature = "bittorrent")]
    piece_stat_man: Arc<PieceStatMan>,
    /// Piece selector for BT downloads (rarest-first by default).
    /// C++ uses `unique_ptr<PieceSelector> pieceSelector_`.
    #[cfg(feature = "bittorrent")]
    piece_selector: PieceSelectorKind,
    /// Stream piece selector for HTTP/FTP downloads.
    /// C++ uses `unique_ptr<StreamPieceSelector> streamPieceSelector_`.
    stream_piece_selector: StreamPieceSelectorKind,
    /// Offset index for Geom stream piece selector.
    /// C++ `GeomStreamPieceSelector::offsetIndex_` — updated by `onBitfieldInit()`
    /// to point to the first missing piece after bitfield initialization.
    geom_offset_index: usize,
    /// Base for Geom stream piece selector geometric progression.
    /// C++ `GeomStreamPieceSelector::base_` — defaults to 1.5.
    geom_base: f64,
    /// In-flight pieces from previous session (used for session resume)
    in_flight_pieces: Vec<Piece>,
}

impl DefaultPieceStorage {
    /// Creates a new DefaultPieceStorage with the given piece length and total length.
    ///
    /// C++ constructor creates `PieceStatMan` with random shuffle and
    /// `RarestPieceSelector` as the default BT piece selector.
    /// Stream piece selector defaults to `Default` (sparse/inorder for HTTP/FTP).
    pub fn new(piece_length: u64, total_length: u64) -> Self {
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            ((total_length + piece_length - 1) / piece_length) as usize
        };

        // C++ initializes PieceStatMan with random shuffle for tie-breaking
        #[cfg(feature = "bittorrent")]
        let piece_stat_man = Arc::new(PieceStatMan::new(num_pieces, true));
        #[cfg(feature = "bittorrent")]
        let piece_selector = PieceSelectorKind::Rarest(
            RarestPieceSelector::new(Arc::clone(&piece_stat_man)),
        );

        DefaultPieceStorage {
            bfman: BitfieldMan::new(piece_length, total_length),
            used_pieces: HashMap::new(),
            end_game: false,
            end_game_piece_num: END_GAME_PIECE_NUM,
            total_length,
            piece_length,
            // C++ starts nextHaveIndex_ at 1, not 0
            next_have_index: 1,
            haves: Vec::new(),
            #[cfg(feature = "bittorrent")]
            piece_stat_man,
            #[cfg(feature = "bittorrent")]
            piece_selector,
            stream_piece_selector: StreamPieceSelectorKind::Default,
            // C++ GeomStreamPieceSelector defaults: base=1.5, offsetIndex=0
            geom_offset_index: 0,
            geom_base: 1.5,
            in_flight_pieces: Vec::new(),
        }
    }

    /// Returns the number of pieces.
    pub fn num_pieces(&self) -> usize {
        self.bfman.num_pieces()
    }

    /// Returns the piece length.
    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    /// Checks if end-game mode should be entered.
    fn check_end_game(&mut self) {
        if !self.end_game && self.bfman.count_missing_pieces() <= self.end_game_piece_num {
            self.end_game = true;
            debug!(
                "Entering end-game mode: {} pieces remaining (threshold: {})",
                self.bfman.count_missing_pieces(),
                self.end_game_piece_num
            );
        }
    }
}

impl PieceStorage for DefaultPieceStorage {
    // ── Piece query ──────────────────────────────────────────────────────

    fn has_missing_unused_piece(&self) -> bool {
        self.bfman.has_missing_piece()
    }

    fn get_missing_piece(
        &mut self,
        min_split_size: u64,
        ignore_bitfield: &[u8],
        _length: u64,
        cuid: u64,
    ) -> Option<Piece> {
        // C++ dispatches to `streamPieceSelector_->select(index, minSplitSize, ignoreBitfield, length)`
        let index = match self.stream_piece_selector {
            StreamPieceSelectorKind::Default => {
                // C++ DefaultStreamPieceSelector: calls getSparseMissingUnusedIndex
                self.bfman.get_sparse_missing_unused_index(min_split_size, ignore_bitfield)
            }
            StreamPieceSelectorKind::Inorder => {
                // C++ InorderStreamPieceSelector: calls getInorderMissingUnusedIndex
                self.bfman.get_inorder_missing_unused_index(
                    0,
                    self.bfman.num_pieces(),
                    min_split_size,
                    ignore_bitfield,
                )
            }
            StreamPieceSelectorKind::Random => {
                // C++ RandomStreamPieceSelector: pick random start, then inorder
                use rand::Rng;
                let num_pieces = self.bfman.num_pieces();
                let start = if num_pieces > 0 {
                    rand::thread_rng().gen_range(0..num_pieces)
                } else {
                    0
                };
                // Try from random start to end
                if let Some(idx) = self.bfman.get_inorder_missing_unused_index(
                    start,
                    num_pieces,
                    min_split_size,
                    ignore_bitfield,
                ) {
                    Some(idx)
                } else if let Some(idx) = self.bfman.get_inorder_missing_unused_index(
                    0,
                    start,
                    min_split_size,
                    ignore_bitfield,
                ) {
                    // Try from beginning to random start
                    Some(idx)
                } else {
                    // Fall back to full inorder (minSplitSize constraint may cause
                    // the two partial searches to miss valid pieces)
                    self.bfman.get_inorder_missing_unused_index(
                        0,
                        num_pieces,
                        min_split_size,
                        ignore_bitfield,
                    )
                }
            }
            StreamPieceSelectorKind::Geom => {
                // C++ GeomStreamPieceSelector: calls getGeomMissingUnusedIndex
                self.bfman.get_geom_missing_unused_index(
                    min_split_size,
                    ignore_bitfield,
                    self.geom_base,
                    self.geom_offset_index,
                )
            }
        };

        let index = index?;
        self.bfman.set_use_piece(index);

        let piece_start = index as u64 * self.piece_length;
        let piece_len = std::cmp::min(self.piece_length, self.total_length.saturating_sub(piece_start));

        let mut piece = Piece::new(index, piece_len);
        piece.add_user(cuid);

        self.used_pieces.insert(index, piece.clone());
        Some(piece)
    }

    fn get_missing_piece_by_index(&mut self, index: usize, cuid: u64) -> Option<Piece> {
        if index >= self.bfman.num_pieces() {
            return None;
        }
        // C++: if(hasPiece(index) || isPieceUsed(index) ||
        //          (bitfieldMan_->isFilterEnabled() && !bitfieldMan_->isFilterBitSet(index)))
        //   return nullptr;
        if self.bfman.has_piece(index)
            || self.bfman.is_use_piece(index)
            || (self.bfman.is_filter_enabled() && !self.bfman.is_filter_bit_set(index))
        {
            return None;
        }

        self.bfman.set_use_piece(index);

        let piece_start = index as u64 * self.piece_length;
        let piece_len = std::cmp::min(self.piece_length, self.total_length.saturating_sub(piece_start));

        let mut piece = Piece::new(index, piece_len);
        piece.add_user(cuid);

        self.used_pieces.insert(index, piece.clone());
        Some(piece)
    }

    fn get_piece(&self, index: usize) -> Option<Piece> {
        // C++ getPiece(index) returns the piece without changing status.
        // If it's in used_pieces, return that; otherwise create a
        // non-checked-out piece (for upload purposes).
        if index >= self.bfman.num_pieces() {
            return None;
        }
        if let Some(piece) = self.used_pieces.get(&index) {
            return Some(piece.clone());
        }
        // Return a new Piece without marking it as used
        let piece_start = index as u64 * self.piece_length;
        let piece_len = std::cmp::min(self.piece_length, self.total_length.saturating_sub(piece_start));
        Some(Piece::new(index, piece_len))
    }

    fn complete_piece(&mut self, piece: &Piece) -> bool {
        let index = piece.index();
        self.used_pieces.remove(&index);
        // C++: if allDownloadFinished(), return early (already complete)
        if PieceStorage::all_download_finished(self) {
            return true;
        }
        self.bfman.set_piece(index);
        self.bfman.unset_use_piece(index);
        // C++: addPieceStats(piece->getIndex()) — increment peer count
        // for this piece in the stats manager
        self.add_piece_stats_for_index(index);
        self.check_end_game();
        true
    }

    fn cancel_piece(&mut self, piece: &mut Piece, cuid: u64) {
        let index = piece.index();
        piece.remove_user(cuid);
        if piece.user_count() == 0 {
            self.bfman.unset_use_piece(index);
        }
        // C++: if not in endgame and piece has 0 completed length, delete used piece
        if !self.end_game && piece.completed_length() == 0 {
            self.used_pieces.remove(&index);
        }
    }

    fn has_piece(&self, index: usize) -> bool {
        self.bfman.has_piece(index)
    }

    fn is_piece_used(&self, index: usize) -> bool {
        self.bfman.is_use_piece(index)
    }

    // ── Length / progress ────────────────────────────────────────────────

    fn get_total_length(&self) -> u64 {
        self.total_length
    }

    fn get_filtered_total_length(&self) -> u64 {
        self.bfman.get_filtered_total_length()
    }

    fn get_completed_length(&self) -> u64 {
        // C++ adds in-flight piece completed lengths, capped at total
        let bfman_completed = self.bfman.get_total_completed_length();
        let in_flight_completed: u64 = self.used_pieces.values().map(|p| p.completed_length()).sum();
        let total = self.total_length;
        std::cmp::min(bfman_completed + in_flight_completed, total)
    }

    fn get_filtered_completed_length(&self) -> u64 {
        self.bfman.get_filtered_completed_length()
            + self.get_in_flight_piece_filtered_completed_length()
    }

    // ── Completion ───────────────────────────────────────────────────────

    fn download_finished(&self) -> bool {
        // C++: `bitfieldMan_->isFilteredAllBitSet()` — if filter is enabled,
        // only filtered pieces need to be complete. Otherwise all pieces.
        self.bfman.is_filtered_all_bit_set()
    }

    fn all_download_finished(&self) -> bool {
        // C++: `bitfieldMan_->isAllBitSet()` — ALL pieces complete, ignoring filter.
        self.bfman.is_all_complete()
    }

    // ── Bitfield ─────────────────────────────────────────────────────────

    fn get_bitfield(&self) -> Vec<u8> {
        self.bfman.bitfield().to_vec()
    }

    fn get_bitfield_length(&self) -> usize {
        self.bfman.bitfield_length()
    }

    fn set_bitfield(&mut self, bitfield: &[u8]) {
        // C++: bitfieldMan_->setBitfield(bitfield, bitfieldLength);
        //      addPieceStats(bitfield, bitfieldLength);
        // C++ setBitfield() also clears the use bitfield.
        self.bfman.clear_all_use_bit();
        self.bfman.set_bitfield(bitfield);
        self.add_piece_stats(bitfield);
        // C++: streamPieceSelector_->onBitfieldInit()
        // GeomStreamPieceSelector uses this to set offsetIndex_ to the
        // first missing piece.
        self.on_bitfield_init();
    }

    // ── Marking ──────────────────────────────────────────────────────────

    fn mark_all_pieces_done(&mut self) {
        // C++: bitfieldMan_->setAllBit()
        self.bfman.set_all_bit();
    }

    fn mark_pieces_done(&mut self, length: u64) {
        // C++: markPiecesDone(length) — if length == total, setAllBit.
        // if length == 0, clearAllBit and clear usedPieces.
        // Otherwise setBitRange for completed pieces and track partial piece.
        if length == self.total_length {
            self.bfman.set_all_bit();
        } else if length == 0 {
            self.bfman.clear_all_bit();
            self.used_pieces.clear();
        } else {
            self.bfman.mark_pieces_done(length);
        }
    }

    fn mark_piece_missing(&mut self, index: usize) {
        if index < self.bfman.num_pieces() && self.bfman.has_piece(index) {
            self.bfman.clear_piece(index);
            trace!("mark_piece_missing: piece {} marked as missing", index);
        }
    }

    // ── End-game ─────────────────────────────────────────────────────────

    fn is_end_game(&self) -> bool {
        self.end_game
    }

    fn enter_end_game(&mut self) {
        self.end_game = true;
    }

    fn set_end_game_piece_num(&mut self, num: usize) {
        self.end_game_piece_num = num;
    }

    // ── Piece length ─────────────────────────────────────────────────────

    fn get_piece_length(&self, index: usize) -> u32 {
        if index >= self.bfman.num_pieces() {
            return 0;
        }
        let piece_start = index as u64 * self.piece_length;
        let remaining = self.total_length.saturating_sub(piece_start);
        std::cmp::min(self.piece_length, remaining) as u32
    }

    // ── Have advertisement ───────────────────────────────────────────────

    fn advertise_piece(&mut self, cuid: u64, index: usize) {
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let entry = HaveEntry {
            have_index: self.next_have_index,
            cuid,
            index,
            registered_time_ms: now_ms,
        };
        self.next_have_index += 1;
        self.haves.push(entry);
    }

    fn get_advertised_piece_indexes(
        &self,
        my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        let mut indexes = Vec::new();
        let mut new_last = last_have_index;
        for entry in &self.haves {
            if entry.have_index > last_have_index && entry.cuid != my_cuid {
                indexes.push(entry.index);
            }
            if entry.have_index >= new_last {
                new_last = entry.have_index + 1;
            }
        }
        (indexes, new_last)
    }

    fn remove_advertised_piece(&mut self, expiry_ms: u64) {
        self.haves.retain(|entry| entry.registered_time_ms > expiry_ms);
    }

    // ── In-flight pieces ─────────────────────────────────────────────────

    fn add_in_flight_piece(&mut self, piece: Piece) {
        self.in_flight_pieces.push(piece);
    }

    fn count_in_flight_piece(&self) -> usize {
        self.in_flight_pieces.len()
    }

    fn get_in_flight_pieces(&self) -> Vec<Piece> {
        self.in_flight_pieces.clone()
    }

    // ── Piece statistics ─────────────────────────────────────────────────

    fn add_piece_stats_for_index(&mut self, index: usize) {
        #[cfg(feature = "bittorrent")]
        {
            self.piece_stat_man.add_piece_stats_index(index);
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = index;
        }
    }

    fn add_piece_stats(&mut self, bitfield: &[u8]) {
        #[cfg(feature = "bittorrent")]
        {
            self.piece_stat_man.add_piece_stats_bitfield(bitfield);
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = bitfield;
        }
    }

    fn subtract_piece_stats(&mut self, bitfield: &[u8]) {
        #[cfg(feature = "bittorrent")]
        {
            self.piece_stat_man.subtract_piece_stats(bitfield);
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = bitfield;
        }
    }

    fn update_piece_stats(&mut self, new_bitfield: &[u8], old_bitfield: &[u8]) {
        #[cfg(feature = "bittorrent")]
        {
            self.piece_stat_man.update_piece_stats(new_bitfield, old_bitfield);
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = (new_bitfield, old_bitfield);
        }
    }

    // ── Navigation ───────────────────────────────────────────────────────

    fn get_next_used_index(&self, index: usize) -> usize {
        for i in (index + 1)..self.bfman.num_pieces() {
            if self.bfman.is_use_piece(i) || self.bfman.has_piece(i) {
                return i;
            }
        }
        self.bfman.num_pieces()
    }

    // ── Lifecycle ────────────────────────────────────────────────────────

    fn on_download_incomplete(&mut self) {
        // C++ sets all used pieces' completed length to 0 and re-checks
        // the bitfield consistency. For now we just log the event.
        trace!("on_download_incomplete: download detected as incomplete");
    }

    // ── Selective downloading ────────────────────────────────────────────

    fn is_selective_downloading_mode(&self) -> bool {
        // C++: `bitfieldMan_->isFilterEnabled()` — returns true when
        // selective downloading (file filtering) is active.
        self.bfman.is_filter_enabled()
    }
}

// ===========================================================================
// DefaultPieceStorage helper methods (shared between trait impls)
// ===========================================================================

impl DefaultPieceStorage {
    /// Helper: returns the sum of completed lengths of in-flight pieces
    /// that intersect the filter ranges.
    /// C++: `getInFlightPieceFilteredCompletedLength()`
    fn get_in_flight_piece_filtered_completed_length(&self) -> u64 {
        let mut len: u64 = 0;
        for piece in self.used_pieces.values() {
            if self.bfman.is_filter_bit_set(piece.index()) {
                len += piece.completed_length();
            }
        }
        len
    }

    /// Helper: increment piece stats for each set bit in the bitfield.
    /// C++: `addPieceStats(bitfield, bitfieldLength)` — delegates to PieceStatMan.
    fn add_piece_stats(&mut self, bitfield: &[u8]) {
        #[cfg(feature = "bittorrent")]
        {
            self.piece_stat_man.add_piece_stats_bitfield(bitfield);
        }
        #[cfg(not(feature = "bittorrent"))]
        {
            let _ = bitfield;
        }
    }

    /// Called after bitfield initialization or update.
    /// C++: `streamPieceSelector_->onBitfieldInit()`
    ///
    /// GeomStreamPieceSelector uses this to set offsetIndex_ to the
    /// first missing piece, so geometric search starts from there.
    fn on_bitfield_init(&mut self) {
        if self.stream_piece_selector == StreamPieceSelectorKind::Geom {
            self.geom_offset_index = self.bfman.get_first_missing_index().unwrap_or(0);
        }
    }

    /// Sets the stream piece selector strategy.
    /// C++: `setStreamPieceSelector(unique_ptr<StreamPieceSelector>)`
    ///
    /// After changing the selector, calls `onBitfieldInit()` so that
    /// Geom selector can update its offset index.
    pub fn set_stream_piece_selector(&mut self, kind: StreamPieceSelectorKind) {
        self.stream_piece_selector = kind;
        self.on_bitfield_init();
    }
}

// ===========================================================================
// PieceProvider implementation for DefaultPieceStorage (BT feature)
// ===========================================================================

/// Implementation of `PieceProvider` for `DefaultPieceStorage`, bridging the
/// BT interaction loop's request generation with the actual piece storage.
///
/// This is the concrete wiring that connects `BtPeerInteractive::add_requests()`
/// to the real piece storage, enabling actual piece downloads.
///
/// # C++ Architecture Reference
///
/// In C++ `DefaultBtInteractive`, `pieceStorage_` is a raw pointer to
/// `PieceStorage` used directly. Rust uses the `PieceProvider` trait for
/// decoupling. This impl provides the bridge.
#[cfg(feature = "bittorrent")]
impl PieceProvider for DefaultPieceStorage {
    fn has_missing_piece(&self, peer: &BtPeerConn) -> bool {
        // C++: bitfieldMan_->hasMissingPiece(peer->getBitfield(), peer->getBitfieldLength())
        let peer_bitfield = match peer.session_resource.as_ref() {
            Some(res) => res.bitfield(),
            None => return false,
        };
        self.bfman.has_missing_piece_with_bitfield(peer_bitfield)
    }

    fn get_missing_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece> {
        let peer_bitfield = match peer.session_resource.as_ref() {
            Some(res) => res.bitfield(),
            None => return Vec::new(),
        };

        self.get_missing_pieces_inner(count, peer_bitfield, target_piece_indexes, cuid, false)
    }

    fn get_missing_fast_pieces(
        &mut self,
        count: usize,
        peer: &BtPeerConn,
        target_piece_indexes: &[u32],
        cuid: u64,
    ) -> Vec<Piece> {
        // Fast pieces: only pieces in the peer's allowed-fast set.
        // C++: createFastIndexBitfield() then select from that.
        let peer_bitfield = match peer.session_resource.as_ref() {
            Some(res) => res.bitfield(),
            None => return Vec::new(),
        };

        self.get_missing_pieces_inner(count, peer_bitfield, target_piece_indexes, cuid, true)
    }

    fn is_end_game(&self) -> bool {
        // Delegate to PieceStorage's implementation
        <Self as PieceStorage>::is_end_game(self)
    }

    fn has_missing_unused_piece(&self) -> bool {
        // Delegate to PieceStorage's implementation
        <Self as PieceStorage>::has_missing_unused_piece(self)
    }

    fn enter_end_game(&mut self) {
        // Delegate to PieceStorage's implementation
        <Self as PieceStorage>::enter_end_game(self)
    }

    // ── checkHave optimization support ────────────────────────────────────

    fn get_advertised_piece_indexes_ext(
        &self,
        my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        // Collect pieces advertised by other CUIDs since last_have_index.
        // C++: iterate haveEntries_ with haveIndex > lastHaveIndex and
        // cuid != my_cuid.
        let mut indexes = Vec::new();
        let mut new_last = last_have_index;
        for entry in &self.haves {
            if entry.have_index > last_have_index && entry.cuid != my_cuid {
                indexes.push(entry.index);
                new_last = new_last.max(entry.have_index);
            }
        }
        (indexes, new_last)
    }

    fn get_bitfield_length_ext(&self) -> usize {
        self.bfman.bitfield().len()
    }

    fn get_bitfield_ext(&self) -> Vec<u8> {
        self.bfman.bitfield().to_vec()
    }

    fn all_download_finished_ext(&self) -> bool {
        self.bfman.is_all_complete()
    }

    fn get_completed_length_ext(&self) -> u64 {
        self.bfman.get_completed_length()
    }
}

#[cfg(feature = "bittorrent")]
impl DefaultPieceStorage {
    /// Internal method to get missing pieces based on the peer's bitfield.
    ///
    /// Mirrors C++ `DefaultPieceStorage::getMissingPiece()`:
    /// - In endgame: get all missing pieces (even in-use), shuffle, pick
    /// - Normal: get missing unused pieces, select via piece selector
    ///
    /// When `fast_only` is true, restrict to pieces in the peer's
    /// allowed-fast set (C++ `createFastIndexBitfield()`).
    fn get_missing_pieces_inner(
        &mut self,
        min_missing_blocks: usize,
        peer_bitfield: &[u8],
        target_piece_indexes: &[u32],
        cuid: u64,
        fast_only: bool,
    ) -> Vec<Piece> {
        let num_pieces = self.bfman.num_pieces();

        // Build a bitfield of pieces we can request from this peer.
        // C++: getAllMissingIndexes() or getAllMissingUnusedIndexes()
        let mis_bitfield = if self.end_game {
            // Endgame: all missing pieces (even in-use by other peers)
            self.bfman.all_missing_indexes(peer_bitfield)
        } else {
            // Normal: only missing unused pieces
            self.bfman.all_missing_unused_indexes(peer_bitfield)
        };

        if mis_bitfield.is_empty() {
            return Vec::new();
        }

        // Exclude pieces already assigned to this peer's request factory
        // C++ passes excludeIndexes to getMissingPiece()
        let mut mis_bitfield = mis_bitfield;
        for &idx in target_piece_indexes {
            let i = idx as usize;
            if i < num_pieces {
                super::bitfield_util::clear_bit(&mut mis_bitfield, num_pieces, i);
            }
        }

        // If fast_only, restrict to allowed-fast pieces
        // C++: createFastIndexBitfield() intersects with peer's allowed set
        // For now, we just use the same bitfield since fast piece filtering
        // would need the peer's allowed-fast index set from BtPeerConn
        if fast_only {
            // TODO: Once we expose the peer's allowed-fast index set from
            // BtPeerConn, intersect mis_bitfield with it here.
            // For now, we use the same bitfield (fast pieces are a subset
            // of available pieces, filtered by the peer's allowed-fast set).
        }

        let mut pieces = Vec::new();
        let mut mis_block = 0usize;

        if self.end_game {
            // Endgame: collect all eligible piece indexes, shuffle, pick
            let mut indexes: Vec<usize> = Vec::new();
            for i in 0..num_pieces {
                if super::bitfield_util::test_bit(&mis_bitfield, num_pieces, i) {
                    indexes.push(i);
                }
            }

            // Shuffle for random distribution (C++ does std::shuffle)
            use rand::seq::SliceRandom;
            let mut rng = rand::thread_rng();
            indexes.shuffle(&mut rng);

            for idx in indexes {
                if mis_block >= min_missing_blocks {
                    break;
                }
                if let Some(piece) = self.check_out_piece(idx, cuid) {
                    mis_block += piece.count_missing_blocks();
                    pieces.push(piece);
                }
            }
        } else {
            // Normal mode: use the piece selector (rarest-first by default).
            // C++ uses `pieceSelector_->select(index, misbitfield, blocks)`.
            // After each selection, flip the bit in mis_bitfield so we don't
            // pick the same piece twice (C++: `bitfield::flipBit`).
            while mis_block < min_missing_blocks {
                match self.piece_selector.select(&mis_bitfield, num_pieces) {
                    Some(index) => {
                        if let Some(piece) = self.check_out_piece(index, cuid) {
                            mis_block += piece.count_missing_blocks();
                            pieces.push(piece);
                            // Flip this bit off so we don't select it again
                            super::bitfield_util::clear_bit(&mut mis_bitfield, num_pieces, index);
                        } else {
                            // Piece was already checked out or not available
                            super::bitfield_util::clear_bit(&mut mis_bitfield, num_pieces, index);
                        }
                    }
                    None => break,
                }
            }
        }

        if !pieces.is_empty() {
            trace!(
                "get_missing_pieces_inner: selected {} pieces ({} missing blocks, fast_only={})",
                pieces.len(),
                mis_block,
                fast_only
            );
        }

        pieces
    }

    /// Check out a piece by index for a given CUID.
    ///
    /// Mirrors C++ `DefaultPieceStorage::checkOutPiece()`.
    /// Marks the piece as in-use in the bitfield and creates a `Piece` object.
    fn check_out_piece(&mut self, index: usize, cuid: u64) -> Option<Piece> {
        if index >= self.bfman.num_pieces() {
            return None;
        }
        if self.bfman.has_piece(index) {
            return None;
        }
        // In endgame, pieces can be in-use (shared across peers)
        if !self.end_game && self.bfman.is_use_piece(index) {
            return None;
        }

        self.bfman.set_use_piece(index);

        let piece_start = index as u64 * self.piece_length;
        let piece_len = std::cmp::min(self.piece_length, self.total_length.saturating_sub(piece_start));

        let mut piece = Piece::new(index, piece_len);
        piece.add_user(cuid);

        // In endgame, the piece might already be in used_pieces
        // C++ handles this by adding another user to the existing piece
        if self.end_game {
            if let Some(existing) = self.used_pieces.get_mut(&index) {
                existing.add_user(cuid);
                return Some(existing.clone());
            }
        }

        self.used_pieces.insert(index, piece.clone());
        Some(piece)
    }
}

// ===========================================================================
// Non-bittorrent stub: PieceProvider is only available with bittorrent feature
// ===========================================================================

// Note: PieceProvider trait requires BtPeerConn which is behind the
// bittorrent feature gate. When bittorrent is disabled, we don't need
// this impl. The trait is still defined (in bt_peer_interaction.rs) but
// not all items may be usable. The _ext methods are provided as inherent
// methods on DefaultPieceStorage for non-bittorrent code paths.

#[cfg(not(feature = "bittorrent"))]
impl DefaultPieceStorage {
    /// Stub for non-bittorrent builds: get bitfield length.
    pub fn get_bitfield_length_ext(&self) -> usize {
        self.bfman.bitfield().len()
    }

    /// Stub for non-bittorrent builds: get bitfield.
    pub fn get_bitfield_ext(&self) -> Vec<u8> {
        self.bfman.bitfield().to_vec()
    }

    /// Stub for non-bittorrent builds: check all download finished.
    pub fn all_download_finished_ext(&self) -> bool {
        self.bfman.is_all_complete()
    }

    /// Stub for non-bittorrent builds: get completed length.
    pub fn get_completed_length_ext(&self) -> u64 {
        self.bfman.get_completed_length()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitfield_man_new() {
        let bfman = BitfieldMan::new(1024 * 1024, 10 * 1024 * 1024);
        assert_eq!(bfman.num_pieces(), 10);
        assert_eq!(bfman.piece_length(), 1024 * 1024);
        assert!(!bfman.is_all_complete());
        assert_eq!(bfman.count_missing_pieces(), 10);
    }

    #[test]
    fn test_bitfield_man_set_and_has_piece() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        assert!(!bfman.has_piece(0));
        bfman.set_piece(0);
        assert!(bfman.has_piece(0));
        assert!(!bfman.has_piece(1));
    }

    #[test]
    fn test_bitfield_man_use_piece() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.set_use_piece(2);
        assert!(bfman.is_use_piece(2));
        assert!(!bfman.is_use_piece(0));
        bfman.unset_use_piece(2);
        assert!(!bfman.is_use_piece(2));
    }

    #[test]
    fn test_bitfield_man_has_missing_piece() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        assert!(bfman.has_missing_piece());
        bfman.set_piece(0);
        bfman.set_piece(1);
        bfman.set_piece(2);
        bfman.set_piece(3);
        assert!(!bfman.has_missing_piece());
    }

    #[test]
    fn test_bitfield_man_mark_pieces_done() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.mark_pieces_done(2048);
        assert!(bfman.has_piece(0));
        assert!(bfman.has_piece(1));
        assert!(!bfman.has_piece(2));
    }

    #[test]
    fn test_bitfield_man_mark_all_done() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.mark_all_done();
        assert!(bfman.is_all_complete());
    }

    #[test]
    fn test_default_piece_storage_new() {
        let storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        assert_eq!(storage.num_pieces(), 10);
        assert!(!storage.download_finished());
        assert!(PieceStorage::has_missing_unused_piece(&storage));
    }

    #[test]
    fn test_default_piece_storage_get_missing_piece() {
        let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        assert_eq!(piece.index(), 0);
        assert!(storage.is_piece_used(0));
        assert!(!storage.has_piece(0));
    }

    #[test]
    fn test_default_piece_storage_complete_piece() {
        let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        let result = storage.complete_piece(&piece);
        assert!(result);
        assert!(storage.has_piece(0));
        assert!(!storage.is_piece_used(0));
    }

    #[test]
    fn test_default_piece_storage_cancel_piece() {
        let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        let mut piece = piece;
        storage.cancel_piece(&mut piece, 1);
        assert!(!storage.is_piece_used(0));
    }

    #[test]
    fn test_default_piece_storage_download_all() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        for _ in 0..4 {
            let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
            storage.complete_piece(&piece);
        }
        assert!(storage.download_finished());
        assert_eq!(storage.get_completed_length(), 4096);
    }

    #[test]
    fn test_default_piece_storage_end_game() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // Complete 2 pieces, leaving 2 remaining
        for _ in 0..2 {
            let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
            storage.complete_piece(&piece);
        }
        // With only 2 pieces remaining (≤ END_GAME_PIECE_NUM), should enter endgame
        assert!(PieceStorage::is_end_game(&storage));
    }

    #[test]
    fn test_bitfield_man_set_bitfield() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        let bitfield = vec![0xFF]; // All 8 bits set
        bfman.set_bitfield(&bitfield);
        assert!(bfman.is_all_complete());
    }

    #[test]
    fn test_bitfield_man_zero_length() {
        let bfman = BitfieldMan::new(0, 0);
        assert_eq!(bfman.num_pieces(), 0);
        assert!(bfman.is_all_complete()); // vacuously true
    }

    // ── New PieceStorage method tests ────────────────────────────────────

    #[test]
    fn test_get_piece_returns_in_flight_piece() {
        let mut storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        let got = storage.get_piece(0).unwrap();
        assert_eq!(got.index(), piece.index());
    }

    #[test]
    fn test_get_piece_returns_new_piece_for_untracked() {
        let storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        let got = storage.get_piece(0).unwrap();
        assert_eq!(got.index(), 0);
        // Should NOT mark it as used
        assert!(!storage.is_piece_used(0));
    }

    #[test]
    fn test_get_piece_out_of_bounds() {
        let storage = DefaultPieceStorage::new(1024 * 1024, 10 * 1024 * 1024);
        assert!(storage.get_piece(999).is_none());
    }

    #[test]
    fn test_get_bitfield_length() {
        let storage = DefaultPieceStorage::new(1024, 4096);
        // 4 pieces → ceil(4/8) = 1 byte
        assert_eq!(storage.get_bitfield_length(), 1);
    }

    #[test]
    fn test_mark_all_pieces_done() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.mark_all_pieces_done();
        assert!(storage.download_finished());
        assert!(storage.has_piece(0));
        assert!(storage.has_piece(3));
    }

    #[test]
    fn test_mark_piece_missing() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.mark_all_pieces_done();
        assert!(storage.has_piece(1));
        storage.mark_piece_missing(1);
        assert!(!storage.has_piece(1));
        assert!(storage.has_piece(0)); // others still done
    }

    #[test]
    fn test_mark_piece_missing_noop_if_already_missing() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // Piece 0 is already missing — should be a no-op
        storage.mark_piece_missing(0);
        assert!(!storage.has_piece(0));
    }

    #[test]
    fn test_set_end_game_piece_num() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.set_end_game_piece_num(2);
        // Complete 2 pieces, leaving 2 remaining
        for _ in 0..2 {
            let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
            storage.complete_piece(&piece);
        }
        // 2 remaining = end_game_piece_num, should enter endgame
        assert!(PieceStorage::is_end_game(&storage));
    }

    #[test]
    fn test_get_piece_length() {
        let storage = DefaultPieceStorage::new(1024, 4096);
        // All pieces are 1024 bytes (total = 4 * 1024, exact multiple)
        assert_eq!(storage.get_piece_length(0), 1024);
        assert_eq!(storage.get_piece_length(3), 1024);
        assert_eq!(storage.get_piece_length(999), 0); // out of bounds
    }

    #[test]
    fn test_get_piece_length_last_piece_shorter() {
        // 3 pieces: [1024, 1024, 512]
        let storage = DefaultPieceStorage::new(1024, 2560);
        assert_eq!(storage.get_piece_length(0), 1024);
        assert_eq!(storage.get_piece_length(1), 1024);
        assert_eq!(storage.get_piece_length(2), 512);
    }

    #[test]
    fn test_advertise_piece_and_get_advertised() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // C++ starts nextHaveIndex_ at 1, so first entry gets have_index=1
        storage.advertise_piece(1, 0); // CUID 1 completed piece 0 (have_index=1)
        storage.advertise_piece(2, 1); // CUID 2 completed piece 1 (have_index=2)

        // CUID 1 asks with last_have_index=0: have_index > 0 matches both
        // → CUID 1's own entry (have_index=1, piece 0) is skipped (same CUID)
        // → CUID 2's entry (have_index=2, piece 1) qualifies
        let (indexes, new_last) = storage.get_advertised_piece_indexes(1, 0);
        assert_eq!(indexes, vec![1]);
        assert_eq!(new_last, 3); // last have_index + 1

        // CUID 2 asks with last_have_index=0: have_index > 0 matches both
        // → CUID 1's entry (have_index=1, piece 0) qualifies
        // → CUID 2's own entry (have_index=2, piece 1) is skipped (same CUID)
        let (indexes2, _) = storage.get_advertised_piece_indexes(2, 0);
        assert_eq!(indexes2, vec![0]);

        // CUID 1 asks with last_have_index=2: only have_index > 2 matches
        // → nothing new since last_have_index
        let (indexes3, _) = storage.get_advertised_piece_indexes(1, 2);
        assert!(indexes3.is_empty());
    }

    #[test]
    fn test_get_advertised_empty() {
        let storage = DefaultPieceStorage::new(1024, 4096);
        let (indexes, new_last) = storage.get_advertised_piece_indexes(1, 0);
        assert!(indexes.is_empty());
        assert_eq!(new_last, 0);
    }

    #[test]
    fn test_remove_advertised_piece() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.advertise_piece(1, 0);
        // Remove all entries older than far future
        storage.remove_advertised_piece(u64::MAX);
        let (indexes, _) = storage.get_advertised_piece_indexes(2, 0);
        assert!(indexes.is_empty());
    }

    #[test]
    fn test_in_flight_pieces() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        let piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.add_in_flight_piece(piece);
        assert_eq!(storage.count_in_flight_piece(), 1);
        let in_flight = storage.get_in_flight_pieces();
        assert_eq!(in_flight.len(), 1);
        assert_eq!(in_flight[0].index(), 0);
    }

    #[test]
    fn test_piece_stats_for_index() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.add_piece_stats_for_index(0);
        storage.add_piece_stats_for_index(0);
        storage.add_piece_stats_for_index(1);
        // Verify bitfield-based subtract works
        let bf = storage.get_bitfield(); // all zeros (no pieces complete)
        storage.subtract_piece_stats(&bf); // no-op since all bits are 0
    }

    #[test]
    fn test_get_next_used_index() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // Complete piece 0 and 2
        let piece0 = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.complete_piece(&piece0);
        let piece2 = storage.get_missing_piece_by_index(2, 1).unwrap();
        storage.complete_piece(&piece2);
        // Next used after 0 should be 2
        assert_eq!(storage.get_next_used_index(0), 2);
        // Next used after 2 should be num_pieces (4)
        assert_eq!(storage.get_next_used_index(2), 4);
    }

    #[test]
    fn test_filtered_lengths_default() {
        let storage = DefaultPieceStorage::new(1024, 4096);
        // No filter active — filtered == unfiltered
        assert_eq!(storage.get_filtered_total_length(), 4096);
        assert_eq!(storage.get_filtered_completed_length(), 0);
    }

    #[test]
    fn test_is_selective_downloading_mode_default() {
        let storage = DefaultPieceStorage::new(1024, 4096);
        assert!(!storage.is_selective_downloading_mode());
    }

    // ── New BitfieldMan method tests ────────────────────────────────────

    #[test]
    fn test_bitfield_man_clear_all_bit() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.set_piece(0);
        bfman.set_piece(1);
        assert_eq!(bfman.count_missing_pieces(), 2);
        bfman.clear_all_bit();
        assert_eq!(bfman.count_missing_pieces(), 4);
        assert!(!bfman.has_piece(0));
    }

    #[test]
    fn test_bitfield_man_set_all_bit() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.set_all_bit();
        assert!(bfman.is_all_complete());
        assert_eq!(bfman.get_completed_length(), 4096);
    }

    #[test]
    fn test_bitfield_man_clear_all_use_bit() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.set_use_piece(0);
        bfman.set_use_piece(2);
        assert!(bfman.is_use_piece(0));
        bfman.clear_all_use_bit();
        assert!(!bfman.is_use_piece(0));
        assert!(!bfman.is_use_piece(2));
    }

    #[test]
    fn test_bitfield_man_disable_filter() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.enable_filter();
        assert!(bfman.is_filter_enabled());
        bfman.disable_filter();
        assert!(!bfman.is_filter_enabled());
    }

    #[test]
    fn test_bitfield_man_clear_filter() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.add_filter(0, 2048);
        bfman.enable_filter();
        assert!(bfman.is_filter_enabled());
        bfman.clear_filter();
        assert!(!bfman.is_filter_enabled());
    }

    #[test]
    fn test_bitfield_man_is_filter_bit_set() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.add_filter(0, 2048);
        bfman.enable_filter();
        assert!(bfman.is_filter_bit_set(0));
        assert!(bfman.is_filter_bit_set(1));
        assert!(!bfman.is_filter_bit_set(2));
        assert!(!bfman.is_filter_bit_set(3));
    }

    #[test]
    fn test_bitfield_man_is_filtered_all_bit_set() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        // No filter: is_filtered_all_bit_set == is_all_complete
        assert!(!bfman.is_filtered_all_bit_set());
        bfman.set_piece(0);
        bfman.set_piece(1);
        bfman.set_piece(2);
        bfman.set_piece(3);
        assert!(bfman.is_filtered_all_bit_set());
    }

    #[test]
    fn test_bitfield_man_is_filtered_all_bit_set_with_filter() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        // Filter: only pieces 0 and 1 are selected
        bfman.add_filter(0, 2048);
        bfman.enable_filter();
        // Nothing completed yet
        assert!(!bfman.is_filtered_all_bit_set());
        // Complete only the filtered pieces
        bfman.set_piece(0);
        bfman.set_piece(1);
        assert!(bfman.is_filtered_all_bit_set());
        // Non-filtered pieces can be incomplete
        assert!(!bfman.has_piece(2));
    }

    #[test]
    fn test_bitfield_man_set_bit_range() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.set_bit_range(1, 2); // pieces 1 and 2
        assert!(!bfman.has_piece(0));
        assert!(bfman.has_piece(1));
        assert!(bfman.has_piece(2));
        assert!(!bfman.has_piece(3));
    }

    #[test]
    fn test_bitfield_man_unset_bit_range() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.set_all_bit();
        bfman.unset_bit_range(1, 2); // clear pieces 1 and 2
        assert!(bfman.has_piece(0));
        assert!(!bfman.has_piece(1));
        assert!(!bfman.has_piece(2));
        assert!(bfman.has_piece(3));
    }

    #[test]
    fn test_bitfield_man_get_last_block_length() {
        // 10 pieces, piece_length=1MB, total=10MB → last piece is full
        let bfman = BitfieldMan::new(1024 * 1024, 10 * 1024 * 1024);
        assert_eq!(bfman.get_last_block_length(), 1024 * 1024);

        // 10 pieces, piece_length=1MB, total=10MB-512KB → last piece is 512KB
        let bfman2 = BitfieldMan::new(1024 * 1024, 10 * 1024 * 1024 - 512 * 1024);
        assert_eq!(bfman2.get_last_block_length(), 512 * 1024);
    }

    #[test]
    fn test_bitfield_man_get_block_length() {
        let bfman = BitfieldMan::new(1024, 3584); // 3 full + 1 of 512
        assert_eq!(bfman.get_block_length(0), 1024);
        assert_eq!(bfman.get_block_length(2), 1024);
        assert_eq!(bfman.get_block_length(3), 512); // last piece
        assert_eq!(bfman.get_block_length(4), 0); // out of range
    }

    #[test]
    fn test_bitfield_man_get_max_index() {
        let bfman = BitfieldMan::new(1024, 4096);
        assert_eq!(bfman.get_max_index(), 3);
        let bfman2 = BitfieldMan::new(1024, 0);
        assert_eq!(bfman2.get_max_index(), 0);
    }

    #[test]
    fn test_bitfield_man_filtered_total_length() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        // No filter: returns total
        assert_eq!(bfman.get_filtered_total_length(), 4096);
        // Enable filter with pieces 0,1 selected
        bfman.add_filter(0, 2048);
        bfman.enable_filter();
        assert_eq!(bfman.get_filtered_total_length(), 2048);
    }

    #[test]
    fn test_bitfield_man_filtered_completed_length() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.add_filter(0, 2048);
        bfman.enable_filter();
        bfman.set_piece(0);
        assert_eq!(bfman.get_filtered_completed_length(), 1024);
        bfman.set_piece(1);
        assert_eq!(bfman.get_filtered_completed_length(), 2048);
    }

    // ── Filter semantics tests (C++ enableFilter doesn't set all bits) ──

    #[test]
    fn test_enable_filter_does_not_set_bits() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.enable_filter();
        // Filter is enabled but no bits are set — nothing is selected
        assert!(bfman.is_filter_enabled());
        assert_eq!(bfman.get_filtered_total_length(), 0);
    }

    #[test]
    fn test_add_filter_then_enable() {
        // C++ setupFileFilter() flow: addFilter for each file, then enableFilter
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.add_filter(0, 2048); // select pieces 0,1
        bfman.enable_filter();
        assert!(bfman.is_filter_bit_set(0));
        assert!(bfman.is_filter_bit_set(1));
        assert!(!bfman.is_filter_bit_set(2));
        assert!(!bfman.is_filter_bit_set(3));
        assert_eq!(bfman.get_filtered_total_length(), 2048);
    }

    #[test]
    fn test_add_not_filter() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        // addNotFilter selects everything EXCEPT the specified range
        bfman.add_not_filter(1024, 1024); // exclude piece 1
        bfman.enable_filter();
        assert!(bfman.is_filter_bit_set(0));  // before range → selected
        assert!(!bfman.is_filter_bit_set(1)); // in range → NOT selected
        assert!(bfman.is_filter_bit_set(2));  // after range → selected
        assert!(bfman.is_filter_bit_set(3));  // after range → selected
    }

    #[test]
    fn test_filter_aware_has_missing_piece() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.add_filter(0, 2048); // only pieces 0,1 are selected
        bfman.enable_filter();
        // Peer has pieces 2,3 — but those are not selected by filter
        let peer_bf: Vec<u8> = vec![0b00110000]; // bits 2,3 set (MSB-first)
        assert!(!bfman.has_missing_piece_with_bitfield(&peer_bf));
        // Peer has pieces 0,1 — those are selected by filter
        let peer_bf2: Vec<u8> = vec![0b11000000]; // bits 0,1 set
        assert!(bfman.has_missing_piece_with_bitfield(&peer_bf2));
    }

    #[test]
    fn test_filter_aware_all_missing_indexes() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        bfman.add_filter(0, 2048); // only pieces 0,1 are selected
        bfman.enable_filter();
        // Peer has all pieces but filter limits to 0,1
        let peer_bf: Vec<u8> = vec![0b11110000];
        let missing = bfman.all_missing_indexes(&peer_bf);
        // Only pieces 0,1 should be in the result
        assert!(super::super::bitfield_util::test_bit(&missing, 4, 0));
        assert!(super::super::bitfield_util::test_bit(&missing, 4, 1));
        assert!(!super::super::bitfield_util::test_bit(&missing, 4, 2));
        assert!(!super::super::bitfield_util::test_bit(&missing, 4, 3));
    }

    // ── DefaultPieceStorage with filter tests ───────────────────────────

    #[test]
    fn test_download_finished_with_filter() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // Set up selective downloading: only pieces 0,1
        storage.bfman.add_filter(0, 2048);
        storage.bfman.enable_filter();

        // Complete only the filtered pieces
        let piece0 = storage.get_missing_piece_by_index(0, 1).unwrap();
        storage.complete_piece(&piece0);
        assert!(!storage.download_finished()); // piece 1 not done

        let piece1 = storage.get_missing_piece_by_index(1, 1).unwrap();
        storage.complete_piece(&piece1);
        assert!(storage.download_finished()); // all filtered pieces done
        assert!(!storage.all_download_finished()); // but not ALL pieces
    }

    #[test]
    fn test_set_bitfield_clears_use_and_adds_stats() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // Mark piece 0 as in-use
        storage.get_missing_piece(0, &[], 0, 1).unwrap();
        assert!(storage.is_piece_used(0));

        // Set bitfield — should clear use bits and update stats
        let bf: Vec<u8> = vec![0b11000000]; // pieces 0,1 complete
        storage.set_bitfield(&bf);
        assert!(!storage.is_piece_used(0)); // use bit cleared
    }

    #[test]
    fn test_mark_pieces_done_zero_clears() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.mark_pieces_done(0);
        assert_eq!(storage.get_completed_length(), 0);
    }

    #[test]
    fn test_mark_pieces_done_full() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.mark_pieces_done(4096);
        assert!(storage.download_finished());
        assert!(storage.all_download_finished());
    }

    // ── Stream piece selector tests ────────────────────────────────────────

    #[test]
    fn test_sparse_selection_basic() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // No ignore, no filter — all 4 pieces available
        let ignore = vec![0u8; 1];

        // First selection: range [0,4), start=0 → return 0
        let piece0 = storage.get_missing_piece(0, &ignore, 0, 1);
        assert!(piece0.is_some());
        assert_eq!(piece0.unwrap().index(), 0);

        // Second selection: piece 0 is in-use → range [1,4)
        // Because piece 0 (before the range start) is in-use,
        // sparse adjusts start to midpoint: (1+4)/2 = 2
        // Then checks: range_size * piece_length >= min_split_size → 2*1024 >= 0 → true
        // Returns piece 2 (C++ behavior: avoid interfering with active download)
        let piece1 = storage.get_missing_piece(0, &ignore, 0, 1);
        assert!(piece1.is_some());
        assert_eq!(piece1.unwrap().index(), 2);
    }

    #[test]
    fn test_inorder_selection_basic() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.set_stream_piece_selector(StreamPieceSelectorKind::Inorder);
        let ignore = vec![0u8; 1];

        // Inorder always returns start_index first
        let piece0 = storage.get_missing_piece(0, &ignore, 0, 1);
        assert_eq!(piece0.unwrap().index(), 0);

        let piece1 = storage.get_missing_piece(0, &ignore, 0, 1);
        assert_eq!(piece1.unwrap().index(), 1);
    }

    #[test]
    fn test_geom_selection_basic() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        storage.set_stream_piece_selector(StreamPieceSelectorKind::Geom);
        let ignore = vec![0u8; 1];

        // Geom starts from offsetIndex=0, searches [0,1), then [1,2.5), etc.
        let piece0 = storage.get_missing_piece(0, &ignore, 0, 1);
        assert_eq!(piece0.unwrap().index(), 0);
    }

    #[test]
    fn test_get_first_missing_index() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        // No pieces complete → first missing is 0
        assert_eq!(bfman.get_first_missing_index(), Some(0));

        // Complete piece 0 → first missing is 1
        bfman.set_piece(0);
        assert_eq!(bfman.get_first_missing_index(), Some(1));

        // Complete all → no missing
        bfman.set_piece(1);
        bfman.set_piece(2);
        bfman.set_piece(3);
        assert_eq!(bfman.get_first_missing_index(), None);
    }

    #[test]
    fn test_get_first_missing_index_with_filter() {
        let mut bfman = BitfieldMan::new(1024, 4096);
        // Only pieces 2,3 are selected by filter
        bfman.add_filter(2048, 2048);
        bfman.enable_filter();
        // First missing with filter = piece 2 (first filter-selected piece)
        assert_eq!(bfman.get_first_missing_index(), Some(2));

        // Complete piece 2 → first missing is 3
        bfman.set_piece(2);
        assert_eq!(bfman.get_first_missing_index(), Some(3));
    }

    #[test]
    fn test_sparse_selection_midpoint() {
        // With pieces completed at both ends, sparse should select midpoint
        let mut bfman = BitfieldMan::new(1024, 8192); // 8 pieces
        let ignore = vec![0u8; 1];

        // Complete pieces 0,1,2 and 6,7 → gap is pieces 3,4,5
        bfman.set_piece(0);
        bfman.set_piece(1);
        bfman.set_piece(2);
        bfman.set_piece(6);
        bfman.set_piece(7);

        // Longest range: [3, 6) → size=3
        // Previous piece (2) is completed and not in-use → return 3
        let result = bfman.get_sparse_missing_unused_index(0, &ignore);
        assert_eq!(result, Some(3));
    }

    #[test]
    fn test_inorder_with_min_split_size() {
        let mut bfman = BitfieldMan::new(1024, 8192); // 8 pieces
        let ignore = vec![0u8; 1];

        // Complete piece 0 → start from piece 1
        bfman.set_piece(0);

        // min_split_size=3072 (3 pieces) → need 3 consecutive free pieces
        // Pieces 1-7 are free → piece 1 is adjacent to completed → return 1
        let result = bfman.get_inorder_missing_unused_index(0, 8, 3072, &ignore);
        assert_eq!(result, Some(1));
    }

    #[test]
    fn test_sparse_with_ignore_bitfield() {
        let bfman = BitfieldMan::new(1024, 4096); // 4 pieces
        // Ignore pieces 2,3
        let ignore: Vec<u8> = vec![0b00110000]; // bits 2,3 set (MSB-first)

        // Only pieces 0,1 are available
        let result = bfman.get_sparse_missing_unused_index(0, &ignore);
        assert_eq!(result, Some(0)); // range [0,2), start=0
    }

    #[test]
    fn test_geom_selection_with_offset() {
        let mut bfman = BitfieldMan::new(1024, 8192); // 8 pieces
        let ignore = vec![0u8; 1];

        // Complete pieces 0,1,2 → offset_index should be 3
        bfman.set_piece(0);
        bfman.set_piece(1);
        bfman.set_piece(2);

        // Geom with offset_index=3, base=1.5
        // Window [3,4) → piece 3 is available → return 3
        let result = bfman.get_geom_missing_unused_index(0, &ignore, 1.5, 3);
        assert_eq!(result, Some(3));
    }

    #[test]
    fn test_get_missing_piece_by_index_filter_check() {
        let mut storage = DefaultPieceStorage::new(1024, 4096);
        // Enable filter for pieces 0,1 only
        storage.bfman.add_filter(0, 2048);
        storage.bfman.enable_filter();

        // Piece 2 is not filter-selected → should return None
        let result = storage.get_missing_piece_by_index(2, 1);
        assert!(result.is_none());

        // Piece 0 is filter-selected → should succeed
        let result = storage.get_missing_piece_by_index(0, 1);
        assert!(result.is_some());
    }

    // ── Range-based query tests (C++ BitfieldMan methods) ──

    #[test]
    fn test_bit_range_set() {
        let mut bf = BitfieldMan::new(1024, 4096);
        // No pieces set → range [0,4) should be false
        assert!(!bf.is_bit_range_set(0, 4));

        // Set pieces 0,1
        bf.set_piece(0);
        bf.set_piece(1);
        assert!(bf.is_bit_range_set(0, 2));
        assert!(!bf.is_bit_range_set(0, 3));

        // Set all pieces
        bf.set_all_bit();
        assert!(bf.is_bit_range_set(0, 4));
    }

    #[test]
    fn test_bit_set_offset_range() {
        let mut bf = BitfieldMan::new(1024, 4096);
        // Empty range should return true
        assert!(bf.is_bit_set_offset_range(0, 0));

        // Set pieces 0,1 → byte range [0, 2048) should be true
        bf.set_piece(0);
        bf.set_piece(1);
        assert!(bf.is_bit_set_offset_range(0, 2048));
        assert!(!bf.is_bit_set_offset_range(0, 4096));
    }

    #[test]
    fn test_offset_completed_length() {
        let mut bf = BitfieldMan::new(1024, 4096);
        // No pieces → completed length = 0
        assert_eq!(bf.get_offset_completed_length(0, 2048), 0);

        // Set pieces 0,1 → range [0, 2048) has 2048 bytes completed
        bf.set_piece(0);
        bf.set_piece(1);
        assert_eq!(bf.get_offset_completed_length(0, 2048), 2048);
        // Range [0, 4096) has 2048 completed (pieces 0,1), 0 for piece 2
        assert_eq!(bf.get_offset_completed_length(0, 4096), 2048);
    }

    #[test]
    fn test_missing_unused_length() {
        let mut bf = BitfieldMan::new(1024, 4096);
        // All 4 pieces missing → 4096 bytes available from index 0
        assert_eq!(bf.get_missing_unused_length(0), 4096);
        // Starting from index 2 → 2048 bytes available
        assert_eq!(bf.get_missing_unused_length(2), 2048);

        // Set piece 0 as in-use → still 4096 from index 1 (piece 0 is used)
        bf.set_use_piece(0);
        assert_eq!(bf.get_missing_unused_length(1), 3072);
    }

    #[test]
    fn test_first_n_missing_unused_indexes() {
        let mut bf = BitfieldMan::new(1024, 4096);
        // Get first 2 missing indexes → [0, 1]
        let indexes = bf.get_first_n_missing_unused_indexes(2);
        assert_eq!(indexes, vec![0, 1]);

        // Get all 4 → [0, 1, 2, 3]
        let indexes = bf.get_first_n_missing_unused_indexes(10);
        assert_eq!(indexes, vec![0, 1, 2, 3]);

        // Set piece 0 → first 2 are [1, 2]
        bf.set_piece(0);
        let indexes = bf.get_first_n_missing_unused_indexes(2);
        assert_eq!(indexes, vec![1, 2]);
    }
}
