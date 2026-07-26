//! SegmentMan struct definition, constructor, basic accessors, and Debug impl.

use std::collections::HashMap;

use crate::segment::piece_storage::BitfieldMan;
use crate::segment::piece_storage::PieceStorage;

use super::TrackingEntry;
use super::peer_stat::PeerStat;

// ===========================================================================
// SegmentMan — The main coordinator (struct definition)
// ===========================================================================

/// Coordinates between PieceStorage and download commands.
///
/// `SegmentMan` is the central point for:
/// - Checking out segments from the piece storage for connections
/// - Tracking which CUID owns which segment
/// - Remembering written lengths for resume support
/// - Managing peer statistics for speed tracking
/// - Managing an ignore bitfield for file-level filtering
///
/// # C++ Reference
///
/// Based on `SegmentMan.h` / `SegmentMan.cc` from aria2 and aria2-next.
/// Key differences from the C++ version:
/// - Uses enum dispatch (`SegmentKind`) instead of virtual dispatch
/// - Uses move semantics instead of `shared_ptr` — segment ownership transfers
///   to the caller; `SegmentMan` tracks only (cuid, index) pairs
/// - Does not depend on `DownloadContext` directly (uses piece_length and
///   total_length fields instead)
/// - WrDiskCache flush support is TODO (will be added with the cache module)
pub struct SegmentMan {
    /// Piece storage backend (optional — set after construction)
    pub(crate) piece_storage: Option<Box<dyn PieceStorage + Send>>,
    /// Lightweight tracking entries for in-flight segments
    pub(crate) used_segment_entries: Vec<TrackingEntry>,
    /// Remembers written length per piece index for resume support
    pub(crate) segment_written_length_memo: HashMap<usize, u64>,
    /// Per-connection download statistics
    pub(crate) peer_stats: Vec<PeerStat>,
    /// Fastest peer stat per server (hostname+protocol)
    pub(crate) fastest_peer_stats: Vec<PeerStat>,
    /// Bitfield for file-level filtering (excluded pieces)
    pub(crate) ignore_bitfield: BitfieldMan,
    /// Nominal piece length from the download context
    pub(crate) piece_length: u64,
    /// Total download length in bytes (0 if unknown)
    pub(crate) total_length: u64,
}

impl SegmentMan {
    /// Creates a new `SegmentMan` with the given piece length and total length.
    ///
    /// The ignore bitfield is initialized with the filter enabled and all
    /// filter bits set (all pieces excluded by default). Call
    /// `recognize_segment_for()` to make specific byte ranges eligible
    /// for download.
    pub fn new(piece_length: u64, total_length: u64) -> Self {
        let mut ignore_bitfield = BitfieldMan::new(piece_length, total_length);
        // Enable filter and set all bits (ignore all segments by default).
        // SegmentMan uses the filter bitfield as an "ignore bitfield":
        // filter bit set = piece ignored (not eligible for download).
        // This is the INVERSE of C++ DefaultPieceStorage's filter semantics
        // where filter bit set = piece included. SegmentMan reuses BitfieldMan's
        // filter infrastructure with inverted meaning.
        ignore_bitfield.enable_filter();
        // Set all filter bits (mark all segments as ignored)
        for byte in ignore_bitfield.get_filter_bitfield_mut() {
            *byte = 0xFF;
        }
        ignore_bitfield.clear_trailing_filter_bits();

        SegmentMan {
            piece_storage: None,
            used_segment_entries: Vec::new(),
            segment_written_length_memo: HashMap::new(),
            peer_stats: Vec::new(),
            fastest_peer_stats: Vec::new(),
            ignore_bitfield,
            piece_length,
            total_length,
        }
    }

    /// Initializes/resets the segment manager state.
    ///
    /// Clears all in-flight segments, written length memos, and peer stats.
    /// Does not reset the piece storage or ignore bitfield.
    pub fn init(&mut self) {
        self.used_segment_entries.clear();
        self.segment_written_length_memo.clear();
        self.peer_stats.clear();
        self.fastest_peer_stats.clear();
    }

    /// Returns the total download length in bytes.
    pub fn total_length(&self) -> u64 {
        match &self.piece_storage {
            Some(ps) => ps.get_total_length(),
            None => self.total_length,
        }
    }

    /// Returns `true` if the download has finished.
    pub fn download_finished(&self) -> bool {
        match &self.piece_storage {
            Some(ps) => ps.download_finished(),
            None => false,
        }
    }

    /// Returns the total completed/downloaded length in bytes.
    pub fn download_length(&self) -> u64 {
        match &self.piece_storage {
            Some(ps) => ps.get_completed_length(),
            None => 0,
        }
    }
}

impl std::fmt::Debug for SegmentMan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentMan")
            .field("piece_length", &self.piece_length)
            .field("total_length", &self.total_length)
            .field("in_flight_count", &self.used_segment_entries.len())
            .field("memo_count", &self.segment_written_length_memo.len())
            .field("peer_stats_count", &self.peer_stats.len())
            .field("all_ignored", &self.all_segments_ignored())
            .finish()
    }
}
