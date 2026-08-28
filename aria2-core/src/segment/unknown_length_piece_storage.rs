//! Piece storage for downloads where the total content length is unknown.
//!
//! `UnknownLengthPieceStorage` treats the entire download as a single piece.
//! This is used for HTTP downloads without a `Content-Length` header (chunked
//! transfer encoding) and similar scenarios where the total size is not known
//! in advance.
//!
//! # Key Semantics
//!
//! - **Single piece**: The entire download is one piece (index 0)
//! - **Single connection**: Only one connection can hold the piece at a time
//! - **No parallelism**: `get_missing_piece()` returns `None` after the first
//!   checkout until the piece is completed or cancelled
//! - **Completion**: When `complete_piece()` is called, the actual downloaded
//!   byte count becomes the `total_length`, and a bitfield is created
//!
//! # C++ Reference
//!
//! Based on `UnknownLengthPieceStorage.h` / `UnknownLengthPieceStorage.cc`.
//! Identical between aria2 original and aria2-next.
//!
//! # Important Differences from DefaultPieceStorage
//!
//! | Feature | DefaultPieceStorage | UnknownLengthPieceStorage |
//! |---------|--------------------|---------------------------|
//! | Piece model | N pieces | Single piece (index 0) |
//! | Parallelism | Multiple connections | One connection only |
//! | Bitfield | Created immediately | Created after completion |
//! | File filtering | Supported | No-op |
//! | End game | Supported | Not supported |
//! | BitTorrent | Supported | All BT methods panic |

use std::sync::Arc;

use tracing::trace;

use super::piece::Piece;
use super::piece_storage::{BitfieldMan, PieceStorage};

// ===========================================================================
// UnknownLengthPieceStorage
// ===========================================================================

/// Piece storage for downloads where the total content length is unknown.
///
/// This implementation treats the entire download as a single piece. When
/// the download completes, the actual byte count becomes the total length
/// and a bitfield is retroactively created.
///
/// # Limitations
///
/// - Only one connection can download at a time (single piece model)
/// - No parallel connections or piece splitting
/// - No file filtering, end-game mode, or BitTorrent support
/// - Ignore bitfield is ignored (single piece, no selection logic)
pub struct UnknownLengthPieceStorage {
    /// The single in-flight piece, if any
    /// C++: `std::shared_ptr<Piece> piece_`
    piece: Option<Piece>,
    /// Bitfield manager — created only after download completion
    /// C++: `std::unique_ptr<BitfieldMan> bitfield_`
    bitfield: Option<BitfieldMan>,
    /// Total download length (0 until completion, then set from piece length)
    /// C++: `int64_t totalLength_`
    total_length: u64,
    /// Piece length (used when creating the bitfield after completion)
    /// C++ reads this from `downloadContext_->getPieceLength()`
    piece_length: u64,
    /// Whether the download has finished
    /// C++: `bool downloadFinished_`
    download_finished: bool,
    /// Disk adaptor for file I/O (optional).
    /// C++: `std::shared_ptr<DirectDiskAdaptor> diskAdaptor_`
    /// Set via `set_disk_adaptor()`. Used by `read_data()` for integrity checking.
    disk_adaptor: Option<Arc<tokio::sync::Mutex<dyn crate::filesystem::disk_adaptor::DiskAdaptor>>>,
}

impl UnknownLengthPieceStorage {
    /// Creates a new `UnknownLengthPieceStorage` with the given piece length.
    ///
    /// The total length is initially 0 (unknown). It will be set when the
    /// download completes.
    pub fn new(piece_length: u64) -> Self {
        trace!(piece_length, "UnknownLengthPieceStorage: created");
        UnknownLengthPieceStorage {
            piece: None,
            bitfield: None,
            total_length: 0,
            piece_length,
            download_finished: false,
            disk_adaptor: None,
        }
    }

    /// Returns the piece length in bytes.
    pub fn piece_length(&self) -> u64 {
        self.piece_length
    }

    /// Set the disk adaptor for file I/O.
    ///
    /// In C++, `UnknownLengthPieceStorage::initStorage()` creates a
    /// `DirectDiskAdaptor` and connects it to a `DefaultDiskWriter`.
    /// Here we use `Arc<Mutex<dyn DiskAdaptor>>` for async-safe shared access.
    pub fn set_disk_adaptor(
        &mut self,
        adaptor: Arc<tokio::sync::Mutex<dyn crate::filesystem::disk_adaptor::DiskAdaptor>>,
    ) {
        self.disk_adaptor = Some(adaptor);
    }

    /// Get a reference to the disk adaptor.
    pub fn get_disk_adaptor(
        &self,
    ) -> Option<&Arc<tokio::sync::Mutex<dyn crate::filesystem::disk_adaptor::DiskAdaptor>>> {
        self.disk_adaptor.as_ref()
    }
}

impl PieceStorage for UnknownLengthPieceStorage {
    fn has_missing_unused_piece(&self) -> bool {
        // There's a missing piece only if the download isn't finished
        // and no one is currently downloading it
        !self.download_finished && self.piece.is_none()
    }

    fn get_missing_piece(
        &mut self,
        _min_split_size: u64,
        _ignore_bitfield: &[u8],
        _length: u64,
        _cuid: u64,
    ) -> Option<Piece> {
        if self.download_finished {
            trace!("UnknownLengthPieceStorage: download already finished");
            return None;
        }

        if self.piece.is_some() {
            trace!("UnknownLengthPieceStorage: piece already checked out");
            return None;
        }

        // Create a single piece for the entire download
        let piece = Piece::new(0, 0);
        self.piece = Some(piece.clone());
        trace!("UnknownLengthPieceStorage: checked out single piece");
        Some(piece)
    }

    fn get_missing_piece_by_index(&mut self, index: usize, _cuid: u64) -> Option<Piece> {
        // Only piece index 0 is valid for unknown-length downloads
        if index != 0 {
            trace!(index, "UnknownLengthPieceStorage: invalid piece index");
            return None;
        }
        self.get_missing_piece(0, &[], 0, 0)
    }

    fn complete_piece(&mut self, piece: &Piece) -> bool {
        if self.download_finished {
            return true;
        }

        // Ignore a late completion for a piece that was cancelled or never
        // checked out. Only the single active piece may complete this store.
        if piece.index() != 0 || self.piece.is_none() {
            trace!(
                piece_index = piece.index(),
                "UnknownLengthPieceStorage: rejected stale piece completion"
            );
            return false;
        }

        // Update total length from the completed piece's actual length
        self.total_length = piece.length();
        self.download_finished = true;

        // Create bitfield retroactively (all bits set = complete)
        if self.total_length > 0 {
            let mut bfman = BitfieldMan::new(self.piece_length, self.total_length);
            bfman.mark_all_done();
            self.bitfield = Some(bfman);
        }

        // Clear the in-flight piece
        self.piece = None;

        trace!(
            total_length = self.total_length,
            "UnknownLengthPieceStorage: download completed"
        );
        true
    }

    fn cancel_piece(&mut self, _piece: &mut Piece, _cuid: u64) {
        // Reset the single piece so it can be checked out again
        if self.piece.is_some() {
            trace!("UnknownLengthPieceStorage: piece cancelled");
            self.piece = None;
        }
    }

    fn has_piece(&self, index: usize) -> bool {
        // Only piece 0 can exist, and it's "complete" only when download is finished
        index == 0 && self.download_finished
    }

    fn is_piece_used(&self, index: usize) -> bool {
        // Piece 0 is "in use" if it's currently checked out
        index == 0 && self.piece.is_some()
    }

    fn get_total_length(&self) -> u64 {
        self.total_length
    }

    fn get_completed_length(&self) -> u64 {
        // C++: if piece_ is not null, returns piece_->getLength() (total piece
        // length, NOT completed_length). Otherwise returns totalLength_.
        // The C++ has a TODO: "we have to return actual completed length here?"
        // Our Rust version returns piece.length() to match C++ behavior.
        if let Some(ref piece) = self.piece {
            piece.length()
        } else {
            self.total_length
        }
    }

    fn download_finished(&self) -> bool {
        self.download_finished
    }

    fn all_download_finished(&self) -> bool {
        self.download_finished
    }

    fn get_bitfield(&self) -> Vec<u8> {
        match &self.bitfield {
            Some(bfman) => bfman.bitfield().to_vec(),
            None => Vec::new(),
        }
    }

    fn set_bitfield(&mut self, _bitfield: &[u8]) {
        // No-op for unknown length (bitfield created on completion)
    }

    fn mark_pieces_done(&mut self, _length: u64) {
        // Not supported for unknown-length downloads
        // C++ calls abort() here, but we just no-op in Rust
    }

    fn is_end_game(&self) -> bool {
        false
    }

    fn enter_end_game(&mut self) {
        // No-op — no end game for unknown-length downloads
    }

    // ── New PieceStorage methods (most are no-ops for unknown-length) ────

    fn get_piece(&self, index: usize) -> Option<Piece> {
        // Return the piece without checkout (for upload).
        // C++ returns an empty Piece for index 0 when no piece is checked out.
        if index != 0 {
            return None;
        }
        match &self.piece {
            Some(p) => Some(p.clone()),
            None => Some(Piece::new(0, 0)),
        }
    }

    fn get_filtered_total_length(&self) -> u64 {
        // No filtering support for unknown-length downloads.
        // C++ returns 0 when filter is not enabled.
        0
    }

    fn get_filtered_completed_length(&self) -> u64 {
        // No filtering support for unknown-length downloads
        self.get_completed_length()
    }

    fn get_bitfield_length(&self) -> usize {
        match &self.bitfield {
            Some(bfman) => bfman.bitfield_length(),
            None => 0,
        }
    }

    fn mark_all_pieces_done(&mut self) {
        // C++: if piece is in-flight, take its length as totalLength;
        // reset the piece, create bitfield, set downloadFinished = true.
        if let Some(ref piece) = self.piece {
            self.total_length = piece.length();
        }
        self.piece = None;

        // Create bitfield retroactively
        if self.total_length > 0 {
            let mut bfman = BitfieldMan::new(self.piece_length, self.total_length);
            bfman.mark_all_done();
            self.bitfield = Some(bfman);
        }

        self.download_finished = true;
    }

    fn mark_piece_missing(&mut self, _index: usize) {
        // Not applicable for unknown-length — no-op
    }

    fn mark_piece_verified(&mut self, _index: usize) {
        // Not applicable for unknown-length — no-op
    }

    fn mark_piece_failed(&mut self, _index: usize) {
        // Not applicable for unknown-length — no-op
    }

    fn read_data(&self, piece_index: usize) -> std::result::Result<Vec<u8>, String> {
        // Only piece 0 is valid
        if piece_index != 0 {
            return Err(format!(
                "Piece index {} out of range for unknown-length storage",
                piece_index
            ));
        }

        // Read piece data from disk via DiskAdaptor.
        // C++: pieceStorage_->getDiskAdaptor()->readData(buf, len, offset)
        if let Some(ref disk_adaptor) = self.disk_adaptor {
            match disk_adaptor.try_lock() {
                Ok(mut adaptor) => {
                    let rt = tokio::runtime::Handle::try_current();
                    match rt {
                        Ok(handle) => match handle.block_on(adaptor.read(0, self.total_length)) {
                            Ok(data) => Ok(data),
                            Err(e) => Err(format!("Disk read error: {}", e)),
                        },
                        Err(_) => {
                            Err("No tokio runtime available for synchronous read".to_string())
                        }
                    }
                }
                Err(_) => Err("Disk adaptor is busy (locked by another task)".to_string()),
            }
        } else {
            Err("UnknownLengthPieceStorage: no disk adaptor connected".to_string())
        }
    }

    fn set_end_game_piece_num(&mut self, _num: usize) {
        // No end-game for unknown-length — no-op
    }

    fn get_piece_length(&self, index: usize) -> u32 {
        if index != 0 {
            return 0;
        }
        self.piece_length as u32
    }

    fn advertise_piece(&mut self, _cuid: u64, _index: usize) {
        // No have advertisement for unknown-length — no-op
    }

    fn get_advertised_piece_indexes(
        &self,
        _my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        // No have advertisement for unknown-length
        (Vec::new(), last_have_index)
    }

    fn remove_advertised_piece(&mut self, _expiry_ms: u64) {
        // No have advertisement for unknown-length — no-op
    }

    fn add_in_flight_piece(&mut self, _piece: Piece) {
        // No in-flight tracking for unknown-length — no-op
    }

    fn count_in_flight_piece(&self) -> usize {
        0
    }

    fn get_in_flight_pieces(&self) -> Vec<Piece> {
        Vec::new()
    }

    fn add_piece_stats_for_index(&mut self, _index: usize) {
        // No piece stats for unknown-length — no-op
    }

    fn add_piece_stats(&mut self, _bitfield: &[u8]) {
        // No piece stats for unknown-length — no-op
    }

    fn subtract_piece_stats(&mut self, _bitfield: &[u8]) {
        // No piece stats for unknown-length — no-op
    }

    fn update_piece_stats(&mut self, _new_bitfield: &[u8], _old_bitfield: &[u8]) {
        // No piece stats for unknown-length — no-op
    }

    fn get_next_used_index(&self, index: usize) -> usize {
        // Single piece model: if index < 0 and piece is in use, return 0; else num_pieces
        if self.piece.is_some() || self.download_finished {
            index + 1
        } else {
            1
        }
    }

    fn on_download_incomplete(&mut self) {
        // No-op for unknown-length
    }

    fn is_selective_downloading_mode(&self) -> bool {
        false
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_storage() {
        let storage = UnknownLengthPieceStorage::new(1024 * 1024);
        assert_eq!(storage.get_total_length(), 0);
        assert!(!storage.download_finished());
        assert!(!storage.all_download_finished());
        assert!(storage.has_missing_unused_piece());
    }

    #[test]
    fn test_checkout_single_piece() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let piece = storage.get_missing_piece(0, &[], 0, 1);
        assert!(piece.is_some());
        let piece = piece.unwrap();
        assert_eq!(piece.index(), 0);
        assert!(storage.is_piece_used(0));
        assert!(!storage.has_missing_unused_piece());
    }

    #[test]
    fn test_checkout_only_once() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let p1 = storage.get_missing_piece(0, &[], 0, 1);
        assert!(p1.is_some());

        // Second checkout should fail
        let p2 = storage.get_missing_piece(0, &[], 0, 2);
        assert!(p2.is_none());
    }

    #[test]
    fn test_cancel_piece_allows_recheckout() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let mut piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        storage.cancel_piece(&mut piece, 1);
        assert!(!storage.is_piece_used(0));
        assert!(storage.has_missing_unused_piece());

        // Can checkout again
        let p2 = storage.get_missing_piece(0, &[], 0, 2);
        assert!(p2.is_some());
    }

    #[test]
    fn test_complete_piece() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let mut piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();

        // Simulate downloading data: set length and mark complete
        piece.set_length(50000);
        piece.reconfigure(50000);
        piece.set_all_blocks();

        // Complete the piece
        let result = storage.complete_piece(&piece);
        assert!(result);
        assert!(storage.download_finished());
        assert_eq!(storage.get_total_length(), 50000);
        assert_eq!(storage.get_completed_length(), 50000);
    }

    #[test]
    fn test_no_checkout_after_completion() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let mut piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        piece.set_length(1000);
        piece.reconfigure(1000);
        piece.set_all_blocks();
        storage.complete_piece(&piece);

        // Can't checkout after completion
        let result = storage.get_missing_piece(0, &[], 0, 2);
        assert!(result.is_none());
        assert!(!storage.has_missing_unused_piece());
    }

    #[test]
    fn test_has_piece() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        assert!(!storage.has_piece(0));

        let mut piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        piece.set_length(500);
        piece.reconfigure(500);
        piece.set_all_blocks();
        storage.complete_piece(&piece);

        assert!(storage.has_piece(0));
        assert!(!storage.has_piece(1));
    }

    #[test]
    fn test_get_bitfield_before_completion() {
        let storage = UnknownLengthPieceStorage::new(1024 * 1024);
        assert!(storage.get_bitfield().is_empty());
    }

    #[test]
    fn test_get_bitfield_after_completion() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let mut piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        piece.set_length(2 * 1024 * 1024);
        piece.reconfigure(2 * 1024 * 1024);
        piece.set_all_blocks();
        storage.complete_piece(&piece);

        // Bitfield should exist with all relevant bits set
        let bf = storage.get_bitfield();
        assert!(!bf.is_empty());
        // With 2 pieces (2MB / 1MB piece_length), first byte should have
        // bits 0 and 1 set: 0b11000000 = 192
        assert_eq!(
            bf[0], 0b11000000,
            "First byte should have bits 0,1 set for 2 pieces"
        );
    }

    #[test]
    fn test_get_missing_piece_by_index() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);

        // Only index 0 is valid
        let piece = storage.get_missing_piece_by_index(0, 1);
        assert!(piece.is_some());

        // Index 1 is invalid
        let mut storage2 = UnknownLengthPieceStorage::new(1024 * 1024);
        let result = storage2.get_missing_piece_by_index(1, 1);
        assert!(result.is_none());
    }

    #[test]
    fn test_set_bitfield_noop() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        storage.set_bitfield(&[0xFF]);
        // Should not change anything (no-op)
        assert!(storage.get_bitfield().is_empty());
    }

    #[test]
    fn test_mark_pieces_done_noop() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        storage.mark_pieces_done(1000);
        // Should not affect anything
        assert_eq!(storage.get_total_length(), 0);
    }

    #[test]
    fn test_end_game_not_supported() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        assert!(!storage.is_end_game());
        storage.enter_end_game();
        assert!(!storage.is_end_game());
    }

    #[test]
    fn test_completed_length_before_checkout() {
        let storage = UnknownLengthPieceStorage::new(1024 * 1024);
        assert_eq!(storage.get_completed_length(), 0);
    }

    #[test]
    fn test_complete_piece_twice() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let mut piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        piece.set_length(1000);
        piece.reconfigure(1000);
        piece.set_all_blocks();
        storage.complete_piece(&piece);

        // Completing again should just return true (already finished)
        let result = storage.complete_piece(&piece);
        assert!(result);
        assert_eq!(storage.get_total_length(), 1000);
    }

    #[test]
    fn test_rejects_completion_without_active_piece() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let mut stale_piece = Piece::new(0, 1000);
        stale_piece.reconfigure(1000);
        stale_piece.set_all_blocks();

        assert!(!storage.complete_piece(&stale_piece));
        assert!(!storage.download_finished());
        assert_eq!(storage.get_total_length(), 0);
        assert!(storage.has_missing_unused_piece());
    }

    #[test]
    fn test_rejects_nonzero_piece_index() {
        let mut storage = UnknownLengthPieceStorage::new(1024 * 1024);
        let active_piece = storage.get_missing_piece(0, &[], 0, 1).unwrap();
        let mut wrong_piece = active_piece;
        wrong_piece.set_index(1);
        wrong_piece.set_length(1000);
        wrong_piece.reconfigure(1000);
        wrong_piece.set_all_blocks();

        assert!(!storage.complete_piece(&wrong_piece));
        assert!(!storage.download_finished());
        assert_eq!(storage.get_total_length(), 0);
        assert!(storage.is_piece_used(0));
    }
}
