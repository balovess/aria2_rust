//! SegmentMan segment lifecycle operations — checkout, cancellation, completion,
//! queries, and private helpers.

use tracing::trace;

use crate::segment::grow_segment::GrowSegment;
use crate::segment::piece::Piece;
use crate::segment::pieced_segment::PiecedSegment;

use super::SegmentMan;
use super::TrackingEntry;
use super::peer_stat::PeerStatus;
use super::segment_kind::SegmentKind;

impl SegmentMan {
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

        let piece = self
            .piece_storage
            .as_mut()
            .and_then(|ps| ps.get_missing_piece(min_split_size, &ignore_bf, bf_len as u64, cuid));

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
                    .is_none_or(|ps| ps.status == PeerStatus::Idle);

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
    /// Delegates to `PieceStorage::complete_piece()`, advertises the
    /// completed piece (propagates Have messages to interested peers),
    /// and removes the tracking entry. Returns `true` if the segment
    /// was found and completed, `false` otherwise.
    ///
    /// # C++ Reference
    ///
    /// `SegmentMan::completeSegment()` calls both
    /// `pieceStorage_->completePiece()` and
    /// `pieceStorage_->advertisePiece(cuid, index, wallclock)`.
    pub fn complete_segment(&mut self, cuid: u64, segment: &SegmentKind) -> bool {
        let piece_index = segment.index();

        // Complete the piece in storage
        if let Some(piece) = segment.piece()
            && let Some(ref mut ps) = self.piece_storage {
                ps.complete_piece(piece);
                // Advertise the completed piece so other commands send Have messages
                // to their peers. C++: pieceStorage_->advertisePiece(cuid, index, wallclock)
                ps.advertise_piece(cuid, piece_index);
            }

        // Remove the tracking entry
        let idx = self
            .used_segment_entries
            .iter()
            .position(|e| e.segment_index == piece_index);

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

    // ── Private helpers ────────────────────────────────────────────────

    /// Computes the number of pieces from piece_length and total_length.
    pub(crate) fn num_pieces(&self) -> usize {
        if self.piece_length == 0 || self.total_length == 0 {
            0
        } else {
            self.total_length.div_ceil(self.piece_length) as usize
        }
    }

    /// Core logic for checking out a segment from a piece.
    ///
    /// 1. Marks piece as `used_by_segment`
    /// 2. Creates `Pieced` segment if piece length > 0, `Grow` otherwise
    /// 3. Records tracking entry (cuid, index)
    /// 4. Checks `segment_written_length_memo` for resume support
    /// 5. Returns the `SegmentKind`
    pub(crate) fn checkout_segment(
        &mut self,
        cuid: u64,
        piece: Option<Piece>,
    ) -> Option<SegmentKind> {
        let mut piece = piece?;
        let piece_index = piece.index();
        let piece_len = piece.length();

        trace!(index = piece_index, cuid, "SegmentMan: attaching segment");

        // TODO: Flush WrDiskCache when implemented

        // Mark piece as used by segment
        piece.set_used_by_segment(true);

        // Create the appropriate segment type
        let mut segment = if piece_len == 0 {
            SegmentKind::Grow(GrowSegment::new())
        } else {
            SegmentKind::Pieced(Box::new(PiecedSegment::new(self.piece_length, piece)))
        };

        trace!(
            index = segment.index(),
            length = segment.length(),
            segment_length = segment.segment_length(),
            written_length = segment.written_length(),
            "SegmentMan: segment checked out"
        );

        // Check written length memo for resume support (C++ behavior)
        if segment.length() > 0
            && let Some(&memo_written) = self.segment_written_length_memo.get(&segment.index()) {
                let current_written = segment.written_length();
                trace!(
                    index = segment.index(),
                    memo_written, current_written, "SegmentMan: checking written length memo"
                );
                // If the memo has more written length than current, and the
                // difference is less than one block, assume those bytes are
                // already downloaded (matching C++ behavior)
                if current_written < memo_written {
                    let block_length = segment.piece().map_or(0, |p| p.block_length() as u64);
                    if block_length > 0 && memo_written - current_written < block_length {
                        segment.update_written_length(memo_written - current_written);
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
    pub(crate) fn cancel_segment_internal(&mut self, cuid: u64, segment_index: usize) {
        trace!(index = segment_index, cuid, "SegmentMan: canceling segment");

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
