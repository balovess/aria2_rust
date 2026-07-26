//! Progress tracking, speed monitoring, and ETA calculation.
//!
//! These methods provide progress queries and speed updates for the
//! download, using the lock-free `AtomicProgress` counters where
//! possible to avoid contention on the `RwLock`.

use std::sync::atomic::Ordering;

use crate::util::rwlock_ext::RwLockRecover;

impl super::RequestGroup {
    // ── Progress Queries ────────────────────────────────────────────────

    /// Total file length in bytes.
    pub fn total_length(&self) -> u64 {
        self.progress.total_length()
    }

    /// Set the total file length.
    pub fn set_total_length(&self, length: u64) {
        self.progress.set_total_length(length);
        tracing::debug!("Setting total length: {} bytes", length);
    }

    /// Completed bytes so far.
    pub fn completed_length(&self) -> u64 {
        self.progress.completed_length()
    }

    /// Total uploaded bytes for BT downloads. Non-BT downloads return 0.
    /// Mirrors C++ `RequestGroup::getUploadLength()`.
    pub fn upload_length(&self) -> u64 {
        self.progress.upload_length()
    }

    /// Update the completed length counter.
    pub fn update_completed_length(&self, length: u64) {
        self.progress.set_completed_length(length);
    }

    /// Update progress from the download engine.
    pub fn update_progress(&self, completed_length: u64) {
        self.progress.set_completed_length(completed_length);
    }

    /// Download progress as a percentage (0.0 - 100.0).
    pub fn progress(&self) -> f64 {
        let total = self.progress.total_length();
        let completed = self.progress.completed_length();

        if total == 0 {
            0.0
        } else {
            (completed as f64 / total as f64) * 100.0
        }
    }

    /// Current download speed in bytes/sec.
    pub fn download_speed(&self) -> u64 {
        self.progress.download_speed()
    }

    /// Current upload speed in bytes/sec.
    pub fn upload_speed(&self) -> u64 {
        self.progress.upload_speed()
    }

    /// Update both download and upload speed counters.
    pub fn update_speed(&self, dl_speed: u64, ul_speed: u64) {
        self.progress.set_download_speed(dl_speed);
        self.progress.set_upload_speed(ul_speed);
    }

    /// Add a segment to the segment list.
    pub fn add_segment(&mut self, segment: crate::segment::Segment) {
        let mut segments = self.segments.recover_mut();
        segments.push(segment);
        tracing::debug!("Adding segment, current segments: {}", segments.len());
    }

    /// Return a snapshot of the current segments.
    pub fn segments(&self) -> Vec<crate::segment::Segment> {
        self.segments.recover().clone()
    }

    /// Time elapsed since download start.
    pub fn elapsed_time(&self) -> Option<std::time::Duration> {
        let start = *self.start_time.recover();
        start.map(|t| t.elapsed())
    }

    /// Estimated time to completion based on current download speed.
    pub fn eta(&self) -> Option<std::time::Duration> {
        let speed = self.progress.download_speed();
        let total = self.progress.total_length();
        let completed = self.progress.completed_length();
        let remaining = total.saturating_sub(completed);

        if speed == 0 || remaining == 0 {
            None
        } else {
            Some(std::time::Duration::from_secs(remaining / speed))
        }
    }

    // ── Atomic Progress Accessors (for session persistence) ─────────────
    // These use AtomicU64 for lock-free reads, suitable for frequent polling.

    /// Set completed length using atomic store (lock-free).
    pub fn set_completed_length(&self, val: u64) {
        self.progress.set_completed_length(val);
    }

    /// Get completed length using atomic load (lock-free).
    pub fn get_completed_length(&self) -> u64 {
        self.progress.completed_length()
    }

    /// Set total length using atomic store (lock-free).
    pub fn set_total_length_atomic(&self, val: u64) {
        self.progress.set_total_length(val);
    }

    /// Get total length using atomic load (lock-free).
    pub fn get_total_length_atomic(&self) -> u64 {
        self.progress.total_length()
    }

    /// Set uploaded length using atomic store (lock-free).
    pub fn set_uploaded_length(&self, val: u64) {
        self.uploaded_length.store(val, Ordering::Relaxed);
    }

    /// Get uploaded length using atomic load (lock-free).
    pub fn get_uploaded_length(&self) -> u64 {
        self.uploaded_length.load(Ordering::Relaxed)
    }

    /// Set download speed cache using atomic store (lock-free).
    pub fn set_download_speed_cached(&self, val: u64) {
        self.progress.set_download_speed(val);
    }

    /// Get download speed from cache using atomic load (lock-free).
    pub fn get_download_speed_cached(&self) -> u64 {
        self.progress.download_speed()
    }

    /// Set upload speed cache using atomic store (lock-free).
    pub fn set_upload_speed_cached(&self, val: u64) {
        self.progress.set_upload_speed(val);
    }

    /// Get upload speed from cache using atomic load (lock-free).
    pub fn get_upload_speed_cached(&self) -> u64 {
        self.progress.upload_speed()
    }

    /// Set resume offset for HTTP/FTP range request resumption.
    pub fn set_resume_offset(&self, offset: u64) {
        self.progress.set_completed_length(offset);
    }

    // ── BT Bitfield ─────────────────────────────────────────────────────

    /// Set BT bitfield (sync, uses std::sync::RwLock).
    pub fn set_bt_bitfield(&self, bf: Option<Vec<u8>>) {
        *self.bt_bitfield.recover_mut() = bf;
    }

    /// Get BT bitfield (sync, uses std::sync::RwLock).
    pub fn get_bt_bitfield(&self) -> Option<Vec<u8>> {
        self.bt_bitfield.recover().clone()
    }

    // ── BT Metadata ─────────────────────────────────────────────────────

    /// Set BT metadata fields (num_pieces, piece_length, info_hash_hex).
    /// Called by BtDownloadCommand after parsing TorrentMeta.
    pub fn set_bt_metadata(&self, num_pieces: u32, piece_length: u32, info_hash_hex: String) {
        self.bt_num_pieces.store(num_pieces, Ordering::Relaxed);
        self.bt_piece_length.store(piece_length, Ordering::Relaxed);
        *self.bt_info_hash_hex.recover_mut() = Some(info_hash_hex);
    }

    /// Get number of pieces (lock-free atomic read).
    pub fn get_bt_num_pieces(&self) -> u32 {
        self.bt_num_pieces.load(Ordering::Relaxed)
    }

    /// Get piece length (lock-free atomic read).
    pub fn get_bt_piece_length(&self) -> u32 {
        self.bt_piece_length.load(Ordering::Relaxed)
    }

    /// Get info hash hex string (blocking read for non-async contexts).
    pub fn get_bt_info_hash_hex(&self) -> Option<String> {
        self.bt_info_hash_hex.recover().clone()
    }
}
