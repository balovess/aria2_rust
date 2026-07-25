//! SegmentMan supporting infrastructure — peer statistics, ignore bitfield,
//! have advertisement (BT), multi-file segment selection, piece counting,
//! and configuration.

use tracing::{debug, trace};

use crate::segment::piece_storage::PieceStorage;
use super::segment_kind::SegmentKind;

use super::peer_stat::{PeerStat, PeerStatus};
use super::SegmentMan;

impl SegmentMan {
    // ── Have advertisement (BT piece propagation) ─────────────────────

    /// Advertises that a piece was completed by the given CUID.
    ///
    /// Delegates to `PieceStorage::advertise_piece()`. Other commands
    /// query the advertised piece indexes to send Have messages to
    /// their interested peers.
    ///
    /// # C++ Reference
    ///
    /// `PieceStorage::advertisePiece(cuid, index, registeredTime)`.
    /// In C++, this is called by `SegmentMan::completeSegment()` and
    /// also directly by callers like `BtDownloadCommand`.
    ///
    /// # Arguments
    ///
    /// * `cuid` — Connection ID that completed the piece
    /// * `index` — Piece index that was completed
    #[cfg(feature = "bittorrent")]
    pub fn advertise_piece(&mut self, cuid: u64, index: usize) {
        if let Some(ref mut ps) = self.piece_storage {
            ps.advertise_piece(cuid, index);
            trace!(
                cuid,
                index,
                "SegmentMan: advertised piece completion"
            );
        }
    }

    /// Gets piece indexes advertised since `last_have_index` by CUIDs
    /// other than `my_cuid`.
    ///
    /// Delegates to `PieceStorage::get_advertised_piece_indexes()`.
    /// Returns a tuple of `(indexes, new_last_have_index)`.
    ///
    /// # C++ Reference
    ///
    /// `PieceStorage::getAdvertisedPieceIndexes(indexes, myCuid, lastHaveIndex)`.
    ///
    /// # Arguments
    ///
    /// * `my_cuid` — Exclude entries from this CUID (our own completions)
    /// * `last_have_index` — Only return entries newer than this index
    #[cfg(feature = "bittorrent")]
    pub fn get_advertised_piece_indexes(
        &self,
        my_cuid: u64,
        last_have_index: u64,
    ) -> (Vec<usize>, u64) {
        match &self.piece_storage {
            Some(ps) => ps.get_advertised_piece_indexes(my_cuid, last_have_index),
            None => (Vec::new(), last_have_index),
        }
    }

    /// Removes have entries older than `expiry_ms` (millis since epoch).
    ///
    /// Delegates to `PieceStorage::remove_advertised_piece()`.
    ///
    /// # C++ Reference
    ///
    /// `PieceStorage::removeAdvertisedPiece(expiry)`.
    #[cfg(feature = "bittorrent")]
    pub fn remove_advertised_piece(&mut self, expiry_ms: u64) {
        if let Some(ref mut ps) = self.piece_storage {
            ps.remove_advertised_piece(expiry_ms);
            trace!(
                expiry_ms,
                "SegmentMan: removed expired have entries"
            );
        }
    }

    // ── Multi-file segment selection ──────────────────────────────────

    /// Gets segments scoped to a specific file entry for multi-file downloads.
    ///
    /// Checkouts segments in the byte range of `[file_offset, file_offset + file_length)`
    /// and pushes them into `segments` until `segments.len() < max_segments` is false.
    /// Segments outside the file entry's range are immediately cancelled.
    ///
    /// # C++ Reference
    ///
    /// `SegmentMan::getSegment(vector<Segment*>&, cuid_t, minSplitSize, FileEntry&, maxSegments)`:
    /// ```cpp
    /// BitfieldMan filter(ignoreBitfield_);
    /// filter.enableFilter();
    /// filter.addNotFilter(fileEntry->getOffset(), fileEntry->getLength());
    /// while(segments.size() < maxSegments) {
    ///   segment = checkoutSegment(cuid,
    ///     pieceStorage_->getMissingPiece(minSplitSize, filter.getFilterBitfield(),
    ///                                     filter.getBitfieldLength(), cuid));
    ///   if(!segment) break;
    ///   if(segment->getPositionToWrite() < fileEntry->getOffset() ||
    ///      fileEntry->getLastOffset() <= segment->getPositionToWrite()) {
    ///     pending.push_back(segment);
    ///   } else {
    ///     segments.push_back(segment);
    ///   }
    /// }
    /// // Cancel pending segments outside the file range
    /// ```
    ///
    /// # Arguments
    ///
    /// * `cuid` — Connection ID requesting segments
    /// * `min_split_size` — Minimum split size for piece selection
    /// * `file_offset` — Byte offset of the file entry in the global stream
    /// * `file_length` — Length of the file entry in bytes
    /// * `max_segments` — Maximum number of segments to return
    ///
    /// # Returns
    ///
    /// A vector of `SegmentKind` scoped to the file entry's byte range.
    /// The caller owns each returned `SegmentKind`.
    pub fn get_segments_for_file_entry(
        &mut self,
        cuid: u64,
        min_split_size: u64,
        file_offset: u64,
        file_length: u64,
        max_segments: usize,
    ) -> Vec<SegmentKind> {
        if max_segments == 0 || file_length == 0 {
            return Vec::new();
        }

        let file_last_offset = file_offset.saturating_add(file_length);

        // Build a combined filter: start from the ignore bitfield, then add
        // a NOT filter for the file entry's range. This selects pieces that
        // are NOT ignored AND fall within the file entry's byte range.
        // C++: BitfieldMan filter(ignoreBitfield_); filter.enableFilter();
        //      filter.addNotFilter(fileEntry->getOffset(), fileEntry->getLength());
        let mut filter = self.ignore_bitfield.clone();
        filter.enable_filter();
        filter.add_not_filter(file_offset, file_length);

        let mut segments: Vec<SegmentKind> = Vec::new();
        let mut pending_indices: Vec<usize> = Vec::new();

        while segments.len() < max_segments {
            let filter_bf = filter.get_filter_bitfield().to_vec();
            let bf_len = filter.get_bitfield_length();

            let piece = self.piece_storage.as_mut().and_then(|ps| {
                ps.get_missing_piece(min_split_size, &filter_bf, bf_len as u64, cuid)
            });

            let segment = match self.checkout_segment(cuid, piece) {
                Some(s) => s,
                None => break, // No more pieces available
            };

            // Check if the segment's write position falls within the file entry
            let pos = segment.position_to_write();
            if pos < file_offset || file_last_offset <= pos {
                // Segment is outside the file entry's range — cancel later
                pending_indices.push(segment.index());
                // We still need to track the segment temporarily for cancellation
                // but don't add it to the result
            } else {
                segments.push(segment);
            }
        }

        // Cancel any segments that were checked out but fall outside the file range.
        // In C++, this calls cancelSegment(cuid, segment) for each pending segment.
        // Since the caller doesn't own these segments, we cancel internally by index.
        for idx in pending_indices {
            self.cancel_segment_by_index(idx);
        }

        if segments.len() < max_segments {
            trace!(
                cuid,
                file_offset,
                file_length,
                got = segments.len(),
                requested = max_segments,
                "SegmentMan: get_segments_for_file_entry returned fewer than requested"
            );
        }

        segments
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
}
