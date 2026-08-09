//! DefaultPieceStorage struct definition, constructor, and basic accessors.

use std::collections::HashMap;
use std::sync::Arc;
use tracing::debug;

use crate::segment::piece::Piece;
use crate::segment::piece_storage::bitfield_man::BitfieldMan;
use crate::segment::piece_storage::types::{
    END_GAME_PIECE_NUM, HaveEntry, StreamPieceSelectorKind,
};

#[cfg(feature = "bittorrent")]
use crate::segment::piece_selector::{PieceSelectorKind, RarestPieceSelector};
#[cfg(feature = "bittorrent")]
use crate::segment::piece_stat_man::PieceStatMan;

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
    pub(crate) end_game_piece_num: usize,
    /// Total length of the download
    pub(crate) total_length: u64,
    /// Piece length in bytes
    pub(crate) piece_length: u64,
    /// Monotonically increasing have-index for HaveEntry ordering
    pub(crate) next_have_index: u64,
    /// Queue of Have entries (advertised piece completions)
    pub(crate) haves: Vec<HaveEntry>,
    /// Piece statistics manager for rarest-first selection.
    /// Shared with PieceSelector via Arc.
    #[cfg(feature = "bittorrent")]
    pub(crate) piece_stat_man: Arc<PieceStatMan>,
    /// Piece selector for BT downloads (rarest-first by default).
    /// C++ uses `unique_ptr<PieceSelector> pieceSelector_`.
    #[cfg(feature = "bittorrent")]
    pub(crate) piece_selector: PieceSelectorKind,
    /// Stream piece selector for HTTP/FTP downloads.
    /// C++ uses `unique_ptr<StreamPieceSelector> streamPieceSelector_`.
    pub(crate) stream_piece_selector: StreamPieceSelectorKind,
    /// Offset index for Geom stream piece selector.
    /// C++ `GeomStreamPieceSelector::offsetIndex_` — updated by `onBitfieldInit()`
    /// to point to the first missing piece after bitfield initialization.
    pub(crate) geom_offset_index: usize,
    /// Base for Geom stream piece selector geometric progression.
    /// C++ `GeomStreamPieceSelector::base_` — defaults to 1.5.
    pub(crate) geom_base: f64,
    /// In-flight pieces from previous session (used for session resume)
    pub(crate) in_flight_pieces: Vec<Piece>,
    /// Disk adaptor for file I/O operations.
    /// C++ uses `unique_ptr<DiskAdaptor> diskAdaptor_`.
    /// Set via `set_disk_adaptor()`. Used by `read_data()` for integrity checking.
    pub(crate) disk_adaptor:
        Option<Arc<tokio::sync::Mutex<dyn crate::filesystem::disk_adaptor::DiskAdaptor>>>,
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
    pub(crate) fn check_end_game(&mut self) {
        if !self.end_game && self.bfman.count_missing_pieces() <= self.end_game_piece_num {
            self.end_game = true;
            debug!(
                "Entering end-game mode: {} pieces remaining (threshold: {})",
                self.bfman.count_missing_pieces(),
                self.end_game_piece_num
            );
        }
    }

    /// Returns completed length of in-flight pieces that are within the filter range.
    /// C++: `DefaultPieceStorage::getInFlightPieceFilteredCompletedLength()`
    pub(crate) fn get_in_flight_piece_filtered_completed_length(&self) -> u64 {
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
    pub(crate) fn add_piece_stats(&mut self, bitfield: &[u8]) {
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
    pub(crate) fn on_bitfield_init(&mut self) {
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
