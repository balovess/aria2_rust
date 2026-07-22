//! Segment manager coordinating between PieceStorage and download commands.
//!
//! `SegmentMan` is the Rust equivalent of the C++ `SegmentMan` class. It
//! coordinates segment checkout/cancellation/completion with the underlying
//! [`PieceStorage`], tracks in-flight segments per CUID, remembers written
//! lengths for resume support, and manages peer statistics.
//!
//! # Architecture
//!
//! ```text
//!   DownloadCommand ──get_segment()──> SegmentMan ──get_missing_piece()──> PieceStorage
//!                      <──SegmentKind──            <──Piece──
//!
//!   DownloadCommand ──complete_segment()──> SegmentMan ──complete_piece()──> PieceStorage
//!   DownloadCommand ──cancel_segment()────> SegmentMan ──cancel_piece()────> PieceStorage
//! ```
//!
//! # Ownership Model
//!
//! Unlike the C++ version which uses `shared_ptr<Segment>` for shared ownership
//! between `SegmentMan` and the caller, this Rust version uses **move semantics**:
//!
//! - `get_segment()` returns `SegmentKind` (Piece owned by caller)
//! - `used_segment_entries` stores lightweight tracking entries `(cuid, index)`
//! - For `cancel_segment(cuid)`, we interact with `PieceStorage` by index
//! - For `complete_segment` / `cancel_segment_by_segment`, the caller passes
//!   the `SegmentKind` reference back
//!
//! # C++ Reference
//!
//! Based on `SegmentMan.h` / `SegmentMan.cc` from both the original aria2
//! and aria2-next. The aria2-next version adds `cancelSegmentByIndex()` and
//! uses `A2_LOG_TRACE` instead of `A2_LOG_DEBUG`.

use std::collections::HashMap;

use tracing::{debug, trace};

use super::grow_segment::GrowSegment;
use super::piece::Piece;
use super::piece_storage::{BitfieldMan, PieceStorage};
use super::pieced_segment::PiecedSegment;

// ===========================================================================
// SegmentKind — Enum dispatch instead of virtual dispatch
// ===========================================================================

/// Enum dispatch for segment types, replacing C++ virtual dispatch.
///
/// The C++ implementation uses a `Segment` base class with virtual methods.
/// This Rust version uses an enum for zero-overhead dispatch and exhaustive
/// pattern matching.
///
/// # Variants
///
/// - [`Pieced`](SegmentKind::Pieced) — Fixed-length piece, wraps a `Piece`
/// - [`Grow`](SegmentKind::Grow) — Unknown-length download (chunked transfer)
#[derive(Debug)]
pub enum SegmentKind {
    /// Fixed-length piece segment (total length known)
    Pieced(PiecedSegment),
    /// Growing segment (total length unknown, e.g. chunked transfer)
    Grow(GrowSegment),
}

impl SegmentKind {
    /// Returns `true` if this segment is fully downloaded.
    pub fn is_complete(&self) -> bool {
        match self {
            SegmentKind::Pieced(p) => p.is_complete(),
            SegmentKind::Grow(g) => g.is_complete(),
        }
    }

    /// Returns the piece index.
    pub fn index(&self) -> usize {
        match self {
            SegmentKind::Pieced(p) => p.index(),
            SegmentKind::Grow(g) => g.index(),
        }
    }

    /// Returns the byte offset of this segment in the file.
    pub fn position(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.position(),
            SegmentKind::Grow(g) => g.position(),
        }
    }

    /// Returns the next byte position to write to.
    pub fn position_to_write(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.position_to_write(),
            SegmentKind::Grow(g) => g.position_to_write(),
        }
    }

    /// Returns the actual length of this segment.
    pub fn length(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.length(),
            SegmentKind::Grow(g) => g.length(),
        }
    }

    /// Returns the nominal segment/piece length.
    pub fn segment_length(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.segment_length(),
            SegmentKind::Grow(g) => g.segment_length(),
        }
    }

    /// Returns how many bytes have been written so far.
    pub fn written_length(&self) -> u64 {
        match self {
            SegmentKind::Pieced(p) => p.written_length(),
            SegmentKind::Grow(g) => g.written_length(),
        }
    }

    /// Increments the written length by `bytes`.
    pub fn update_written_length(&mut self, bytes: u64) {
        match self {
            SegmentKind::Pieced(p) => p.update_written_length(bytes),
            SegmentKind::Grow(g) => g.update_written_length(bytes),
        }
    }

    /// Updates the hash computation with data at the given offset.
    pub fn update_hash(&mut self, begin: u64, data: &[u8]) -> bool {
        match self {
            SegmentKind::Pieced(p) => p.update_hash(begin, data),
            SegmentKind::Grow(g) => g.update_hash(begin, data),
        }
    }

    /// Returns `true` if the hash has been fully computed.
    pub fn is_hash_calculated(&self) -> bool {
        match self {
            SegmentKind::Pieced(p) => p.is_hash_calculated(),
            SegmentKind::Grow(g) => g.is_hash_calculated(),
        }
    }

    /// Returns the hash digest as a hex string, or empty if unavailable.
    pub fn digest(&mut self) -> String {
        match self {
            SegmentKind::Pieced(p) => p.digest(),
            SegmentKind::Grow(g) => g.digest(),
        }
    }

    /// Returns a reference to the underlying piece, if any.
    ///
    /// Returns `None` for grow segments.
    pub fn piece(&self) -> Option<&Piece> {
        match self {
            SegmentKind::Pieced(p) => Some(p.piece()),
            SegmentKind::Grow(_) => None,
        }
    }

    /// Returns a mutable reference to the underlying piece, if any.
    ///
    /// Returns `None` for grow segments.
    pub fn piece_mut(&mut self) -> Option<&mut Piece> {
        match self {
            SegmentKind::Pieced(p) => Some(p.piece_mut()),
            SegmentKind::Grow(_) => None,
        }
    }
}

impl PartialEq for SegmentKind {
    fn eq(&self, other: &Self) -> bool {
        self.index() == other.index()
    }
}

impl Eq for SegmentKind {}

// ===========================================================================
// TrackingEntry — Lightweight in-flight segment tracker
// ===========================================================================

/// Lightweight tracking entry for in-flight segments.
///
/// Unlike the C++ version which stores `shared_ptr<Segment>`, this Rust
/// version only stores the CUID and piece index. The actual `SegmentKind`
/// (with the `Piece`) is owned by the caller (download command).
///
/// When the caller needs to cancel or complete a segment, they pass the
/// `SegmentKind` back to `SegmentMan`. For `cancel_segment(cuid)` (which
/// cancels ALL segments for a CUID without the caller passing segments back),
/// we interact with `PieceStorage` by index.
#[derive(Debug, Clone)]
struct TrackingEntry {
    /// Connection ID that owns this segment
    cuid: u64,
    /// Piece index of the in-flight segment
    segment_index: usize,
}

// ===========================================================================
// PeerStat — Lightweight per-connection download statistics
// ===========================================================================

/// Status of a peer/connection for download speed tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStatus {
    /// Actively downloading data
    Active,
    /// Not currently downloading (idle)
    Idle,
}

/// Lightweight per-connection download statistics.
///
/// This is a simplified version of the full `PeerStats` from the engine
/// module. `SegmentMan` needs its own tracking for:
/// - Looking up peer status by CUID (for `get_clean_segment_if_owner_is_idle`)
/// - Tracking the fastest peer per server (for connection optimization)
#[derive(Debug, Clone)]
pub struct PeerStat {
    /// Connection ID
    pub cuid: u64,
    /// Current download speed in bytes/sec
    pub download_speed: u64,
    /// Average download speed in bytes/sec
    pub avg_download_speed: u64,
    /// Session download length in bytes
    pub session_download_length: u64,
    /// Server hostname
    pub hostname: String,
    /// Protocol (e.g., "http", "https", "ftp")
    pub protocol: String,
    /// Current status (active or idle)
    pub status: PeerStatus,
}

impl PeerStat {
    /// Creates a new `PeerStat` with the given CUID, hostname, and protocol.
    pub fn new(cuid: u64, hostname: String, protocol: String) -> Self {
        PeerStat {
            cuid,
            download_speed: 0,
            avg_download_speed: 0,
            session_download_length: 0,
            hostname,
            protocol,
            status: PeerStatus::Idle,
        }
    }

    /// Adds `length` bytes to the session download counter.
    pub fn add_session_download_length(&mut self, length: u64) {
        self.session_download_length = self.session_download_length.saturating_add(length);
    }
}

// ===========================================================================
// SegmentMan — The main coordinator
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
    piece_storage: Option<Box<dyn PieceStorage + Send>>,
    /// Lightweight tracking entries for in-flight segments
    used_segment_entries: Vec<TrackingEntry>,
    /// Remembers written length per piece index for resume support
    segment_written_length_memo: HashMap<usize, u64>,
    /// Per-connection download statistics
    peer_stats: Vec<PeerStat>,
    /// Fastest peer stat per server (hostname+protocol)
    fastest_peer_stats: Vec<PeerStat>,
    /// Bitfield for file-level filtering (excluded pieces)
    ignore_bitfield: BitfieldMan,
    /// Nominal piece length from the download context
    piece_length: u64,
    /// Total download length in bytes (0 if unknown)
    total_length: u64,
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

    // ── Segment checkout ───────────────────────────────────────────────

    /// Gets the next missing segment for the given CUID.
    ///
    /// Requests a missing piece from the piece storage and checks it out
    /// as a segment. The segment is recorded in `used_segment_entries`.
    ///
    /// # Arguments
    ///
    /// * `cuid` — Connection ID requesting the segment
    /// * `min_split_size` — Minimum split size for piece selection
    ///
    /// # Returns
    ///
    /// `Some(SegmentKind)` if a segment was available, `None` otherwise.
    /// The caller owns the returned `SegmentKind` and must pass it back
    /// to `complete_segment()` or `cancel_segment_by_segment()`.
    pub fn get_segment(&mut self, cuid: u64, min_split_size: u64) -> Option<SegmentKind> {
        let ignore_bf = self.ignore_bitfield.get_filter_bitfield().to_vec();
        let bf_len = self.ignore_bitfield.get_bitfield_length();

        let piece = self.piece_storage.as_mut().and_then(|ps| {
            ps.get_missing_piece(min_split_size, &ignore_bf, bf_len as u64, cuid)
        });

        self.checkout_segment(cuid, piece)
    }

    /// Gets a segment with a specific piece index for the given CUID.
    ///
    /// If the index is out of range, returns `None`.
    pub fn get_segment_with_index(&mut self, cuid: u64, index: usize) -> Option<SegmentKind> {
        if index > 0 && self.num_pieces() <= index {
            return None;
        }

        let piece = self
            .piece_storage
            .as_mut()
            .and_then(|ps| ps.get_missing_piece_by_index(index, cuid));

        self.checkout_segment(cuid, piece)
    }

    /// Gets a clean (zero written length) segment if its owner is idle.
    ///
    /// If the segment at `index` exists and has zero written length:
    /// - If the current CUID already owns it, return it
    /// - If the owner is idle, cancel the owner's segment and re-acquire
    /// - Otherwise, return `None`
    pub fn get_clean_segment_if_owner_is_idle(
        &mut self,
        cuid: u64,
        index: usize,
    ) -> Option<SegmentKind> {
        if index > 0 && self.num_pieces() <= index {
            return None;
        }

        // Look for an existing entry with this index
        let owner_cuid = self
            .used_segment_entries
            .iter()
            .find(|e| e.segment_index == index)
            .map(|e| e.cuid);

        match owner_cuid {
            Some(owner_cuid) => {
                // Check if the owner is idle
                let owner_is_idle = self
                    .get_peer_stat(owner_cuid)
                    .map_or(true, |ps| ps.status == PeerStatus::Idle);

                if owner_cuid == cuid {
                    // Same CUID already owns it
                    return self.get_segment_with_index(cuid, index);
                }

                if owner_is_idle {
                    // Cancel the idle owner's segment and acquire it
                    self.cancel_segment(owner_cuid);
                    return self.get_segment_with_index(cuid, index);
                }

                None
            }
            None => None,
        }
    }

    // ── Segment cancellation ───────────────────────────────────────────

    /// Cancels all segments for the given CUID.
    ///
    /// Each cancelled segment's piece is returned to the piece storage.
    /// Since the caller owns the actual `SegmentKind`, we interact with
    /// `PieceStorage` directly by index.
    pub fn cancel_segment(&mut self, cuid: u64) {
        let mut i = 0;
        while i < self.used_segment_entries.len() {
            if self.used_segment_entries[i].cuid == cuid {
                let entry = self.used_segment_entries.remove(i);
                self.cancel_segment_internal(cuid, entry.segment_index);
                // Don't increment i — the next element shifted into position i
            } else {
                i += 1;
            }
        }
    }

    /// Cancels a specific segment for the given CUID.
    ///
    /// Uses the caller's `SegmentKind` to accurately memoize the written
    /// length for resume support, then cancels the piece in storage.
    pub fn cancel_segment_by_segment(&mut self, cuid: u64, segment: &SegmentKind) {
        let idx = self
            .used_segment_entries
            .iter()
            .position(|e| e.cuid == cuid && e.segment_index == segment.index());

        if let Some(i) = idx {
            let entry = self.used_segment_entries.remove(i);
            // Memoize the written length from the caller's segment
            // (always memoize, even if 0 — C++ behavior)
            let written = segment.written_length();
            self.segment_written_length_memo
                .insert(entry.segment_index, written);
            trace!(
                index = entry.segment_index,
                written_length = written,
                "SegmentMan: memoized written length on cancel"
            );
            // Mark piece as not used by segment and cancel in storage
            if let Some(ref mut ps) = self.piece_storage {
                if let Some(piece_mut) = segment.piece() {
                    let mut temp_piece = piece_mut.clone();
                    temp_piece.set_used_by_segment(false);
                    ps.cancel_piece(&mut temp_piece, cuid);
                } else {
                    // Grow segment — just cancel by creating minimal piece
                    let mut temp_piece = Piece::new(entry.segment_index, 0);
                    temp_piece.add_user(cuid);
                    ps.cancel_piece(&mut temp_piece, cuid);
                }
            }
        }
    }

    /// Cancels any segment with the given piece index, regardless of CUID.
    ///
    /// Returns `true` if a segment was found and cancelled, `false` otherwise.
    /// This is the aria2-next addition not present in the original aria2.
    pub fn cancel_segment_by_index(&mut self, index: usize) -> bool {
        let idx = self
            .used_segment_entries
            .iter()
            .position(|e| e.segment_index == index);

        if let Some(i) = idx {
            let entry = self.used_segment_entries.remove(i);
            self.cancel_segment_internal(entry.cuid, entry.segment_index);
            true
        } else {
            false
        }
    }

    /// Cancels all in-flight segments.
    pub fn cancel_all_segments(&mut self) {
        let entries: Vec<TrackingEntry> = self.used_segment_entries.drain(..).collect();
        for entry in entries {
            self.cancel_segment_internal(entry.cuid, entry.segment_index);
        }
    }

    // ── Segment completion ─────────────────────────────────────────────

    /// Marks a segment as completed.
    ///
    /// Delegates to `PieceStorage::complete_piece()` and removes the
    /// tracking entry. Returns `true` if the segment was found and
    /// completed, `false` otherwise.
    pub fn complete_segment(&mut self, _cuid: u64, segment: &SegmentKind) -> bool {
        // Complete the piece in storage
        if let Some(piece) = segment.piece() {
            if let Some(ref mut ps) = self.piece_storage {
                ps.complete_piece(piece);
            }
        }

        // Remove the tracking entry
        let idx = self
            .used_segment_entries
            .iter()
            .position(|e| e.segment_index == segment.index());

        match idx {
            Some(i) => {
                self.used_segment_entries.remove(i);
                true
            }
            None => false,
        }
    }

    // ── Segment queries ────────────────────────────────────────────────

    /// Returns `true` if the piece at the given index has been downloaded.
    pub fn has_segment(&self, index: usize) -> bool {
        match &self.piece_storage {
            Some(ps) => ps.has_piece(index),
            None => false,
        }
    }

    /// Returns all in-flight segment indices for the given CUID.
    pub fn get_in_flight_segment_indices(&self, cuid: u64) -> Vec<usize> {
        self.used_segment_entries
            .iter()
            .filter(|e| e.cuid == cuid)
            .map(|e| e.segment_index)
            .collect()
    }

    // ── Peer statistics ────────────────────────────────────────────────

    /// Registers a peer stat for download speed tracking.
    pub fn register_peer_stat(&mut self, stat: PeerStat) {
        // Replace existing idle stat with the same CUID, or append
        if let Some(existing) = self
            .peer_stats
            .iter_mut()
            .find(|p| p.cuid == stat.cuid && p.status == PeerStatus::Idle)
        {
            *existing = stat;
        } else {
            self.peer_stats.push(stat);
        }
    }

    /// Returns the peer stat for the given CUID, if any.
    pub fn get_peer_stat(&self, cuid: u64) -> Option<&PeerStat> {
        self.peer_stats.iter().find(|p| p.cuid == cuid)
    }

    /// Updates the fastest peer stat tracking for the given peer's server.
    pub fn update_fastest_peer_stat(&mut self, stat: &PeerStat) {
        let existing = self
            .fastest_peer_stats
            .iter_mut()
            .find(|p| p.hostname == stat.hostname && p.protocol == stat.protocol);

        match existing {
            Some(fastest) => {
                if fastest.avg_download_speed < stat.avg_download_speed {
                    // New peer is faster — accumulate old session length into new
                    let mut new_fastest = stat.clone();
                    new_fastest.add_session_download_length(fastest.session_download_length);
                    *fastest = new_fastest;
                } else {
                    // Existing is still faster — accumulate new peer's session length
                    fastest.add_session_download_length(stat.session_download_length);
                }
            }
            None => {
                self.fastest_peer_stats.push(stat.clone());
            }
        }
    }

    /// Returns a reference to the fastest peer stats.
    pub fn fastest_peer_stats(&self) -> &[PeerStat] {
        &self.fastest_peer_stats
    }

    /// Returns a reference to all peer stats.
    pub fn peer_stats(&self) -> &[PeerStat] {
        &self.peer_stats
    }

    // ── Ignore bitfield (file-level filtering) ─────────────────────────

    /// Excludes segments covering the given byte range from selection.
    pub fn ignore_segment_for(&mut self, offset: u64, length: u64) {
        debug!(
            offset,
            length, "SegmentMan: ignoring segment range"
        );
        self.ignore_bitfield.add_filter(offset, length);
    }

    /// Includes segments covering the given byte range in selection.
    pub fn recognize_segment_for(&mut self, offset: u64, length: u64) {
        debug!(
            offset,
            length, "SegmentMan: recognizing segment range"
        );
        self.ignore_bitfield.remove_filter(offset, length);
    }

    /// Returns `true` if all segments are filtered (ignored).
    pub fn all_segments_ignored(&self) -> bool {
        self.ignore_bitfield.is_all_filter_bit_set()
    }

    // ── Piece counting ─────────────────────────────────────────────────

    /// Counts free (not downloaded and not in-use) pieces starting from `index`.
    pub fn count_free_piece_from(&self, index: usize) -> usize {
        let num_pieces = self.num_pieces();
        let ps = match &self.piece_storage {
            Some(ps) => ps,
            None => return 0,
        };

        for i in index..num_pieces {
            if ps.has_piece(i) || ps.is_piece_used(i) {
                return i - index;
            }
        }
        num_pieces - index
    }

    // ── Configuration ──────────────────────────────────────────────────

    /// Sets the piece storage backend.
    pub fn set_piece_storage(&mut self, ps: Box<dyn PieceStorage + Send>) {
        self.piece_storage = Some(ps);
    }

    /// Clears the written length memo (used after download is complete).
    pub fn erase_segment_written_length_memo(&mut self) {
        self.segment_written_length_memo.clear();
    }

    // ── Private helpers ────────────────────────────────────────────────

    /// Computes the number of pieces from piece_length and total_length.
    fn num_pieces(&self) -> usize {
        if self.piece_length == 0 || self.total_length == 0 {
            0
        } else {
            ((self.total_length + self.piece_length - 1) / self.piece_length) as usize
        }
    }

    /// Core logic for checking out a segment from a piece.
    ///
    /// 1. Marks piece as `used_by_segment`
    /// 2. Creates `Pieced` segment if piece length > 0, `Grow` otherwise
    /// 3. Records tracking entry (cuid, index)
    /// 4. Checks `segment_written_length_memo` for resume support
    /// 5. Returns the `SegmentKind`
    fn checkout_segment(&mut self, cuid: u64, piece: Option<Piece>) -> Option<SegmentKind> {
        let mut piece = piece?;
        let piece_index = piece.index();
        let piece_len = piece.length();

        trace!(
            index = piece_index,
            cuid,
            "SegmentMan: attaching segment"
        );

        // TODO: Flush WrDiskCache when implemented

        // Mark piece as used by segment
        piece.set_used_by_segment(true);

        // Create the appropriate segment type
        let mut segment = if piece_len == 0 {
            SegmentKind::Grow(GrowSegment::new())
        } else {
            SegmentKind::Pieced(PiecedSegment::new(self.piece_length, piece))
        };

        trace!(
            index = segment.index(),
            length = segment.length(),
            segment_length = segment.segment_length(),
            written_length = segment.written_length(),
            "SegmentMan: segment checked out"
        );

        // Check written length memo for resume support (C++ behavior)
        if segment.length() > 0 {
            if let Some(&memo_written) = self.segment_written_length_memo.get(&segment.index()) {
                let current_written = segment.written_length();
                trace!(
                    index = segment.index(),
                    memo_written,
                    current_written,
                    "SegmentMan: checking written length memo"
                );
                // If the memo has more written length than current, and the
                // difference is less than one block, assume those bytes are
                // already downloaded (matching C++ behavior)
                if current_written < memo_written {
                    let block_length = segment
                        .piece()
                        .map_or(0, |p| p.block_length() as u64);
                    if block_length > 0 && memo_written - current_written < block_length {
                        segment.update_written_length(memo_written - current_written);
                    }
                }
            }
        }

        // Record the tracking entry
        self.used_segment_entries.push(TrackingEntry {
            cuid,
            segment_index: piece_index,
        });

        Some(segment)
    }

    /// Internal cancel logic — marks piece as not used by segment and
    /// cancels in piece storage.
    ///
    /// Since we don't have the caller's `SegmentKind` here (only the index),
    /// we create a temporary `Piece` for the `cancel_piece` call. This works
    /// correctly for the common case (one CUID per piece). For the rare
    /// multi-CUID case (end-game mode), there may be minor inaccuracies
    /// that self-correct on the next checkout.
    fn cancel_segment_internal(&mut self, cuid: u64, segment_index: usize) {
        trace!(
            index = segment_index,
            cuid,
            "SegmentMan: canceling segment"
        );

        if let Some(ref mut ps) = self.piece_storage {
            // Create a minimal Piece with just the index for cancel_piece.
            // We add the cuid as a user so that cancel_piece can remove it.
            let mut temp_piece = Piece::new(segment_index, 0);
            temp_piece.add_user(cuid);
            temp_piece.set_used_by_segment(false);
            ps.cancel_piece(&mut temp_piece, cuid);
        }

        // Memoize written length as 0 (we don't have the actual value
        // from the caller's SegmentKind). This is conservative: the next
        // checkout will use the Piece's completed_length() as the starting
        // point, which is based on block-level tracking and is accurate.
        self.segment_written_length_memo.insert(segment_index, 0);
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

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::piece_storage::DefaultPieceStorage;

    /// Helper: create a SegmentMan with a DefaultPieceStorage.
    fn create_segment_man(piece_length: u64, total_length: u64) -> SegmentMan {
        let mut man = SegmentMan::new(piece_length, total_length);
        let storage = DefaultPieceStorage::new(piece_length, total_length);
        man.set_piece_storage(Box::new(storage));
        man
    }

    // ── Construction ────────────────────────────────────────────────────

    #[test]
    fn test_new_initializes_ignore_bitfield() {
        let man = SegmentMan::new(1024 * 1024, 10 * 1024 * 1024);
        // All segments should be ignored by default (filter enabled, all bits set)
        assert!(man.all_segments_ignored());
        assert_eq!(man.total_length(), 10 * 1024 * 1024);
    }

    #[test]
    fn test_init_clears_state() {
        let mut man = create_segment_man(1024, 4096);
        man.register_peer_stat(PeerStat::new(1, "host".to_string(), "http".to_string()));
        man.init();
        assert!(man.peer_stats().is_empty());
        assert!(man.used_segment_entries.is_empty());
    }

    // ── Segment checkout ────────────────────────────────────────────────

    #[test]
    fn test_get_segment_returns_pieced_segment() {
        let mut man = create_segment_man(1024 * 1024, 10 * 1024 * 1024);
        // Recognize a range so pieces are selectable
        man.recognize_segment_for(0, 10 * 1024 * 1024);

        let segment = man.get_segment(1, 0);
        assert!(segment.is_some());

        let seg = segment.unwrap();
        assert_eq!(seg.index(), 0);
        assert_eq!(seg.length(), 1024 * 1024);
        assert_eq!(seg.position(), 0);
        assert!(!seg.is_complete());
    }

    #[test]
    fn test_get_segment_returns_none_when_all_done() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        // Checkout and complete all 4 pieces
        for _ in 0..4 {
            let seg = man.get_segment(1, 0).unwrap();
            man.complete_segment(1, &seg);
        }

        // No more segments available
        assert!(man.get_segment(1, 0).is_none());
    }

    #[test]
    fn test_get_segment_with_index() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        let seg = man.get_segment_with_index(1, 2);
        assert!(seg.is_some());
        assert_eq!(seg.unwrap().index(), 2);
    }

    #[test]
    fn test_get_segment_with_index_out_of_range() {
        let man = create_segment_man(1024, 4096);
        let mut man = man;
        man.recognize_segment_for(0, 4096);

        assert!(man.get_segment_with_index(1, 10).is_none());
    }

    // ── Segment cancellation ────────────────────────────────────────────

    #[test]
    fn test_cancel_segment_by_cuid() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        // Checkout two segments for CUID 1
        // With Default (sparse) stream selector:
        // - First: piece 0 (range [0,4), start=0)
        // - Second: piece 2 (range [1,4), adjusted to midpoint because piece 0 is in-use)
        let seg1 = man.get_segment(1, 0).unwrap();
        let seg2 = man.get_segment(1, 0).unwrap();
        assert_eq!(seg1.index(), 0);
        assert_eq!(seg2.index(), 2); // sparse midpoint

        // Cancel all segments for CUID 1
        man.cancel_segment(1);

        // The pieces should be available again
        let seg3 = man.get_segment(2, 0).unwrap();
        assert_eq!(seg3.index(), 0); // Piece 0 was released
    }

    #[test]
    fn test_cancel_segment_by_segment() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        let seg = man.get_segment(1, 0).unwrap();
        assert_eq!(seg.index(), 0);

        // Cancel the specific segment
        man.cancel_segment_by_segment(1, &seg);

        // Piece should be available again
        let seg2 = man.get_segment(2, 0).unwrap();
        assert_eq!(seg2.index(), 0);
    }

    #[test]
    fn test_cancel_segment_by_index() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        let seg = man.get_segment(1, 0).unwrap();
        assert_eq!(seg.index(), 0);

        // Cancel by piece index
        let cancelled = man.cancel_segment_by_index(0);
        assert!(cancelled);

        // Piece should be available again
        let seg2 = man.get_segment(2, 0).unwrap();
        assert_eq!(seg2.index(), 0);

        // Since we just checked out piece 0 for CUID 2,
        // cancel_segment_by_index(0) should succeed again
        let cancelled2 = man.cancel_segment_by_index(0);
        assert!(cancelled2);

        // No more entries for piece 0 — should return false
        let cancelled3 = man.cancel_segment_by_index(0);
        assert!(!cancelled3);
    }

    #[test]
    fn test_cancel_all_segments() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        let _seg1 = man.get_segment(1, 0).unwrap();
        let _seg2 = man.get_segment(1, 0).unwrap();

        assert_eq!(man.used_segment_entries.len(), 2);
        man.cancel_all_segments();
        assert!(man.used_segment_entries.is_empty());
    }

    // ── Segment completion ──────────────────────────────────────────────

    #[test]
    fn test_complete_segment() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        let seg = man.get_segment(1, 0).unwrap();
        let result = man.complete_segment(1, &seg);
        assert!(result);
        assert!(man.has_segment(0));
        assert!(man.used_segment_entries.is_empty());
    }

    // ── Download progress ───────────────────────────────────────────────

    #[test]
    fn test_download_finished() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        assert!(!man.download_finished());

        for i in 0..4 {
            let seg = man.get_segment(1, 0).unwrap();
            assert_eq!(seg.index(), i);
            man.complete_segment(1, &seg);
        }

        assert!(man.download_finished());
    }

    #[test]
    fn test_download_length() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        assert_eq!(man.download_length(), 0);

        let seg = man.get_segment(1, 0).unwrap();
        man.complete_segment(1, &seg);

        assert_eq!(man.download_length(), 1024);
    }

    // ── Peer statistics ─────────────────────────────────────────────────

    #[test]
    fn test_register_and_get_peer_stat() {
        let mut man = create_segment_man(1024, 4096);
        let stat = PeerStat::new(42, "example.com".to_string(), "http".to_string());
        man.register_peer_stat(stat);

        let found = man.get_peer_stat(42);
        assert!(found.is_some());
        assert_eq!(found.unwrap().hostname, "example.com");

        assert!(man.get_peer_stat(99).is_none());
    }

    #[test]
    fn test_update_fastest_peer_stat() {
        let mut man = create_segment_man(1024, 4096);

        let mut stat1 = PeerStat::new(1, "host".to_string(), "http".to_string());
        stat1.avg_download_speed = 1000;
        stat1.session_download_length = 5000;
        man.update_fastest_peer_stat(&stat1);

        let mut stat2 = PeerStat::new(2, "host".to_string(), "http".to_string());
        stat2.avg_download_speed = 2000;
        stat2.session_download_length = 3000;
        man.update_fastest_peer_stat(&stat2);

        // stat2 is faster, so it should replace stat1
        // but session_download_length should be accumulated
        let fastest = &man.fastest_peer_stats()[0];
        assert_eq!(fastest.avg_download_speed, 2000);
        assert_eq!(fastest.session_download_length, 8000); // 5000 + 3000
    }

    // ── Ignore bitfield ─────────────────────────────────────────────────

    #[test]
    fn test_ignore_and_recognize_segments() {
        let mut man = create_segment_man(1024, 4096);
        // By default all segments are ignored
        assert!(man.all_segments_ignored());

        // Recognize a range
        man.recognize_segment_for(0, 2048);
        assert!(!man.all_segments_ignored());

        // Ignore it again
        man.ignore_segment_for(0, 2048);
        assert!(man.all_segments_ignored());
    }

    #[test]
    fn test_get_segment_respects_ignore_bitfield() {
        let mut man = create_segment_man(1024, 4096);
        // By default all segments are ignored — get_segment should return None
        assert!(man.get_segment(1, 0).is_none());

        // Recognize one piece
        man.recognize_segment_for(0, 1024);
        let seg = man.get_segment(1, 0);
        assert!(seg.is_some());
        assert_eq!(seg.unwrap().index(), 0);
    }

    // ── Written length memo ─────────────────────────────────────────────

    #[test]
    fn test_erase_segment_written_length_memo() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        let seg = man.get_segment(1, 0).unwrap();
        man.cancel_segment_by_segment(1, &seg);
        assert_eq!(man.segment_written_length_memo.len(), 1);

        man.erase_segment_written_length_memo();
        assert!(man.segment_written_length_memo.is_empty());
    }

    // ── Count free pieces ───────────────────────────────────────────────

    #[test]
    fn test_count_free_piece_from() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        assert_eq!(man.count_free_piece_from(0), 4);

        let _seg = man.get_segment(1, 0).unwrap();
        // Piece 0 is in-use but not completed
        assert_eq!(man.count_free_piece_from(0), 0);
    }

    // ── In-flight segment indices ───────────────────────────────────────

    #[test]
    fn test_get_in_flight_segment_indices() {
        let mut man = create_segment_man(1024, 4096);
        man.recognize_segment_for(0, 4096);

        // With Default (sparse) stream selector:
        // - First: piece 0
        // - Second: piece 2 (midpoint because piece 0 is in-use)
        // - Third for CUID 2: piece 1 or 3 (depends on remaining ranges)
        let _seg1 = man.get_segment(1, 0).unwrap();
        let _seg2 = man.get_segment(1, 0).unwrap();
        let _seg3 = man.get_segment(2, 0).unwrap();

        let cuid1_indices = man.get_in_flight_segment_indices(1);
        // CUID 1 got pieces 0 and 2
        assert_eq!(cuid1_indices, vec![0, 2]);

        let cuid2_indices = man.get_in_flight_segment_indices(2);
        // CUID 2 got the next available piece
        assert!(!cuid2_indices.is_empty());
    }

    // ── Full download lifecycle ─────────────────────────────────────────

    #[test]
    fn test_full_download_lifecycle() {
        let mut man = create_segment_man(1024 * 1024, 5 * 1024 * 1024);
        man.recognize_segment_for(0, 5 * 1024 * 1024);

        // Simulate downloading all 5 pieces.
        // Order depends on the stream piece selector strategy.
        // Sparse selector may not return pieces in sequential order.
        let mut downloaded_indices = Vec::new();
        for _ in 0..5 {
            let mut seg = man.get_segment(1, 0).unwrap();
            downloaded_indices.push(seg.index());
            assert!(!seg.is_complete());

            // Simulate writing data
            seg.update_written_length(1024 * 1024);
            assert!(seg.is_complete());

            // Complete the segment
            let result = man.complete_segment(1, &seg);
            assert!(result);
        }

        // All 5 distinct pieces should have been downloaded
        assert_eq!(downloaded_indices.len(), 5);
        downloaded_indices.sort();
        assert_eq!(downloaded_indices, vec![0, 1, 2, 3, 4]);

        assert!(man.download_finished());
        assert_eq!(man.download_length(), 5 * 1024 * 1024);
    }
}
