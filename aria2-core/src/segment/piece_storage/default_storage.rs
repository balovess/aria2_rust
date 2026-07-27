//! DefaultPieceStorage — Default implementation of PieceStorage.
//!
//! Uses `BitfieldMan` for piece tracking and supports piece selection strategies.
//! Mirrors C++ `DefaultPieceStorage`.

use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, trace};

use super::super::piece::Piece;
use super::bitfield_man::BitfieldMan;
use super::trait_def::PieceStorage;
use super::types::{END_GAME_PIECE_NUM, HaveEntry, StreamPieceSelectorKind};

#[cfg(feature = "bittorrent")]
use super::super::piece_selector::{PieceSelectorKind, RarestPieceSelector};
#[cfg(feature = "bittorrent")]
use super::super::piece_stat_man::PieceStatMan;

/// Default implementation of PieceStorage for HTTP/FTP and BitTorrent downloads.
///
/// Uses `BitfieldMan` for piece tracking and supports piece selection strategies.
/// Mirrors C++ `DefaultPieceStorage`.
pub struct DefaultPieceStorage {
    /// Bitfield manager for piece tracking
    pub(crate) bfman: BitfieldMan,
    /// Pieces currently in-flight (index -> Piece)
    pub(crate) used_pieces: HashMap<usize, Piece>,
    /// Whether we are in end-game mode
    pub(crate) end_game: bool,
    /// Number of remaining pieces that trigger end-game mode
    end_game_piece_num: usize,
    /// Total length of the download
    pub(crate) total_length: u64,
    /// Piece length in bytes
    pub(crate) piece_length: u64,
    /// Monotonically increasing have-index for HaveEntry ordering
    next_have_index: u64,
    /// Queue of Have entries (advertised piece completions)
    pub(crate) haves: Vec<HaveEntry>,
    /// Piece statistics manager for rarest-first selection.
    /// Shared with PieceSelector via Arc.
    #[cfg(feature = "bittorrent")]
    piece_stat_man: Arc<PieceStatMan>,
    /// Piece selector for BT downloads (rarest-first by default).
    /// C++ uses `unique_ptr<PieceSelector> pieceSelector_`.
    #[cfg(feature = "bittorrent")]
    pub(crate) piece_selector: PieceSelectorKind,
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
    /// Disk adaptor for file I/O operations.
    /// C++ uses `unique_ptr<DiskAdaptor> diskAdaptor_`.
    /// Set via `set_disk_adaptor()`. Used by `read_data()` for integrity checking.
    disk_adaptor: Option<Arc<tokio::sync::Mutex<dyn crate::filesystem::disk_adaptor::DiskAdaptor>>>,
}

impl DefaultPieceStorage {
    /// Creates a new DefaultPieceStorage with the given piece length and total length.
    ///
    /// C++ constructor creates `PieceStatMan` with random shuffle and
    /// `RarestPieceSelector` as the default BT piece selector.
    /// Stream piece selector defaults to `Default` (sparse/inorder for HTTP/FTP).
    pub fn new(piece_length: u64, total_length: u64) -> Self {
        #[allow(unused_variables)]
        let num_pieces = if piece_length == 0 || total_length == 0 {
            0
        } else {
            total_length.div_ceil(piece_length) as usize
        };

        // C++ initializes PieceStatMan with random shuffle for tie-breaking
        #[cfg(feature = "bittorrent")]
        let piece_stat_man = Arc::new(PieceStatMan::new(num_pieces, true));
        #[cfg(feature = "bittorrent")]
        let piece_selector =
            PieceSelectorKind::Rarest(RarestPieceSelector::new(Arc::clone(&piece_stat_man)));

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
            disk_adaptor: None,
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

    /// Set the disk adaptor for file I/O.
    ///
    /// In C++, `PieceStorage` owns a `unique_ptr<DiskAdaptor>` set during
    /// construction. Here we use `Arc<Mutex<dyn DiskAdaptor>>` for async-safe
    /// shared access. The disk adaptor is used by `read_data()` for integrity
    /// checking and by other I/O operations.
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
                self.bfman
                    .get_sparse_missing_unused_index(min_split_size, ignore_bitfield)
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
        let piece_len = std::cmp::min(
            self.piece_length,
            self.total_length.saturating_sub(piece_start),
        );

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
        let piece_len = std::cmp::min(
            self.piece_length,
            self.total_length.saturating_sub(piece_start),
        );

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
        let piece_len = std::cmp::min(
            self.piece_length,
            self.total_length.saturating_sub(piece_start),
        );
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
        let in_flight_completed: u64 = self
            .used_pieces
            .values()
            .map(|p| p.completed_length())
            .sum();
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

    fn mark_piece_verified(&mut self, index: usize) {
        // Verified pieces are already marked as complete by complete_piece().
        // This method ensures the bit is set (idempotent).
        if index < self.bfman.num_pieces() && !self.bfman.has_piece(index) {
            self.bfman.set_piece(index);
            trace!(
                "mark_piece_verified: piece {} verified and marked complete",
                index
            );
        }
    }

    fn mark_piece_failed(&mut self, index: usize) {
        // Failed hash check — mark the piece as missing so it will be re-downloaded.
        // Equivalent to mark_piece_missing() but with different trace semantics.
        if index < self.bfman.num_pieces() && self.bfman.has_piece(index) {
            self.bfman.clear_piece(index);
            trace!(
                "mark_piece_failed: piece {} hash check failed, marked for re-download",
                index
            );
        }
    }

    fn read_data(&self, piece_index: usize) -> std::result::Result<Vec<u8>, String> {
        if piece_index >= self.bfman.num_pieces() {
            return Err(format!(
                "Piece index {} out of range (max {})",
                piece_index,
                self.bfman.num_pieces()
            ));
        }

        // Calculate piece offset and length.
        let piece_offset = piece_index as u64 * self.piece_length;
        let piece_len = std::cmp::min(
            self.piece_length,
            self.total_length.saturating_sub(piece_offset),
        );

        // Read piece data from disk via DiskAdaptor.
        // In C++: pieceStorage_->getDiskAdaptor()->readData(buf, len, offset)
        // NOTE: This is a synchronous interface. The async DiskAdaptor requires
        // a tokio runtime to be available. For now, we attempt a blocking read
        // via tokio::task::block_in_place. If no disk adaptor is available,
        // return an error — the integrity checker will treat this as a failed piece.
        if let Some(ref disk_adaptor) = self.disk_adaptor {
            // We need to perform async I/O synchronously. Use try_lock to avoid
            // deadlocks, falling back to an error if the adaptor is busy.
            match disk_adaptor.try_lock() {
                Ok(mut adaptor) => {
                    // Use tokio runtime to perform the async read synchronously.
                    // This is safe in the context of integrity checking which is
                    // inherently sequential.
                    let rt = tokio::runtime::Handle::try_current();
                    match rt {
                        Ok(handle) => {
                            match handle.block_on(adaptor.read(piece_offset, piece_len)) {
                                Ok(data) => Ok(data),
                                Err(e) => Err(format!(
                                    "Disk read error at offset {}: {}",
                                    piece_offset, e
                                )),
                            }
                        }
                        Err(_) => {
                            Err("No tokio runtime available for synchronous read".to_string())
                        }
                    }
                }
                Err(_) => Err("Disk adaptor is busy (locked by another task)".to_string()),
            }
        } else {
            Err("No disk adaptor connected to DefaultPieceStorage".to_string())
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
        _my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        let mut indexes = Vec::new();
        // C++ header documentation states that indexes should be filtered
        // by CUID, but the C++ implementation (DefaultPieceStorage.cc line 731-733)
        // does NOT filter by myCuid — it only checks haveIndex > lastHaveIndex.
        // We match the C++ implementation behavior exactly.
        for entry in &self.haves {
            if entry.have_index > last_have_index {
                indexes.push(entry.index);
            }
        }
        // C++ returns the haveIndex of the last entry in the vector
        // (i.e., the maximum haveIndex), or lastHaveIndex if no entries match.
        let new_last = self.haves.last().map_or(last_have_index, |e| e.have_index);
        (indexes, new_last)
    }

    fn remove_advertised_piece(&mut self, expiry_ms: u64) {
        self.haves
            .retain(|entry| entry.registered_time_ms > expiry_ms);
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
            self.piece_stat_man
                .update_piece_stats(new_bitfield, old_bitfield);
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
