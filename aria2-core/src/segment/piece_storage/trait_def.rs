//! PieceStorage trait definition.
//!
//! This is the Rust equivalent of the C++ `PieceStorage` abstract class.
//! Methods are aligned with the C++ interface; BT-specific peer-overloaded
//! methods live in the separate `PieceProvider` trait.

use super::super::piece::Piece;

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

    /// Mark a piece as verified after successful hash check.
    ///
    /// In C++ `IteratableChunkChecksumValidator`, this sets the bit in the
    /// validation bitfield. In Rust, this sets the completion bit for the
    /// piece so it won't be re-downloaded.
    fn mark_piece_verified(&mut self, index: usize);

    /// Mark a piece as failed after hash check mismatch.
    ///
    /// Equivalent to `mark_piece_missing()` — clears the completion bit
    /// so the piece will be re-downloaded.
    fn mark_piece_failed(&mut self, index: usize);

    /// Read the data for a piece from disk.
    ///
    /// In C++, this calls `getDiskAdaptor()->readData(buf, len, offset)`.
    /// Returns the piece data as a `Vec<u8>`, or an error if the read fails.
    ///
    /// This is used by `PieceHashValidator::validate_chunk()` to read piece
    /// data for hash verification during integrity checking.
    fn read_data(&self, piece_index: usize) -> std::result::Result<Vec<u8>, String>;

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
    fn get_advertised_piece_indexes(&self, my_cuid: u64, last_have_index: u64)
    -> (Vec<usize>, u64);

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

    /// Set up the file filter for selective downloading.
    ///
    /// C++: `setupFileFilter()` — iterates `downloadContext_->getFileEntries()`
    /// and calls `addFilter()` for each requested file's byte range.
    /// After setting filter bits, calls `enableFilter()`.
    ///
    /// In Rust, this is called by the download engine when selective
    /// downloading is configured (e.g., downloading specific files from
    /// a multi-file torrent).
    fn setup_file_filter(&mut self);

    /// Clear the file filter, disabling selective downloading.
    ///
    /// C++: `clearFileFilter()` — calls `bitfieldMan_->clearFilter()`.
    fn clear_file_filter(&mut self);
}
