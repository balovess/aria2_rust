//! Segment for downloads where the total size is unknown.
//!
//! `GrowSegment` is used for downloads where the total content length is not
//! known in advance (e.g., chunked transfer encoding, server without
//! Content-Length header). Unlike a regular [`Segment`](super::Segment) which
//! has a fixed known length, a `GrowSegment` starts with length 0 and grows
//! as data arrives.
//!
//! # Key Semantics
//!
//! - The segment is **never** considered "complete" in the traditional sense —
//!   the download finishes when the connection closes or the server signals
//!   end-of-stream.
//! - `position_to_write()` always equals `written_length()` — data is written
//!   sequentially from offset 0.
//! - `index()`, `position()`, `length()`, and `segment_length()` all return 0
//!   because there is no fixed piece index, file offset, or known total size.
//! - Hash computation is not supported (`update_hash` returns `false`,
//!   `is_hash_calculated` returns `false`, `digest` returns an empty string).
//!
//! # C++ Reference
//!
//! Based on `GrowSegment.h` / `GrowSegment.cc` from the original aria2
//! implementation. The C++ version wraps a `Piece` whose bitfield is
//! reconfigured on each `updateWrittenLength` call and then marked all-complete
//! (since all downloaded data is complete by definition). This Rust version
//! captures the same semantics without storing a redundant bitfield — the
//! "piece" is implicitly always fully downloaded up to `written_length`.

use tracing::trace;

/// Segment for downloads where the total size is unknown.
///
/// # Examples
///
/// ```
/// use aria2_core::segment::GrowSegment;
///
/// let mut seg = GrowSegment::new();
/// assert_eq!(seg.written_length(), 0);
/// assert!(!seg.is_complete());
///
/// seg.update_written_length(1024);
/// assert_eq!(seg.written_length(), 1024);
/// assert_eq!(seg.position_to_write(), 1024);
///
/// seg.update_written_length(2048);
/// assert_eq!(seg.written_length(), 3072);
/// assert!(!seg.is_complete()); // Never completes
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrowSegment {
    /// How much data has been downloaded so far.
    written_length: u64,
}

impl GrowSegment {
    /// Creates a new `GrowSegment` with zero written length.
    pub fn new() -> Self {
        GrowSegment { written_length: 0 }
    }

    /// Returns `false` always.
    ///
    /// A grow segment is never "complete" in the traditional sense because
    /// the total size is unknown. The download finishes when the connection
    /// closes or the server signals end-of-stream.
    pub fn is_complete(&self) -> bool {
        false
    }

    /// Returns the piece index, which is always `0`.
    ///
    /// In the original C++ implementation, `getIndex()` returns 0 because a
    /// grow segment does not map to a specific piece in a known-size file.
    pub fn index(&self) -> usize {
        0
    }

    /// Returns the file position, which is always `0`.
    ///
    /// In the original C++ implementation, `getPosition()` returns 0 because
    /// the segment starts at the beginning of the download.
    pub fn position(&self) -> u64 {
        0
    }

    /// Returns the next position to write to.
    ///
    /// This equals `written_length()` — data is written sequentially from
    /// offset 0, so the next write position is always the current end of
    /// downloaded data.
    pub fn position_to_write(&self) -> u64 {
        self.written_length
    }

    /// Returns the segment length, which is always `0`.
    ///
    /// The segment length is unknown because the total download size is not
    /// known in advance. In the C++ implementation, `getLength()` and
    /// `getSegmentLength()` both return 0.
    pub fn length(&self) -> u64 {
        0
    }

    /// Returns the total segment length, which is always `0`.
    ///
    /// Alias for [`length()`](Self::length) — both return 0 because the total
    /// size is unknown.
    pub fn segment_length(&self) -> u64 {
        0
    }

    /// Returns how much data has been downloaded so far.
    pub fn written_length(&self) -> u64 {
        self.written_length
    }

    /// Increments the written length by `bytes`.
    ///
    /// In the C++ implementation, this also reconfigures the wrapped `Piece`
    /// to the new length and marks all its blocks as complete (since all data
    /// up to the new written length is downloaded). In this Rust version, the
    /// piece is implicitly always fully downloaded up to `written_length`.
    ///
    /// # Arguments
    ///
    /// * `bytes` - Number of bytes to add to the written length. Must be
    ///   non-negative; callers should not pass values that would cause
    ///   `written_length` to overflow `u64::MAX`.
    pub fn update_written_length(&mut self, bytes: u64) {
        trace!(
            previous = self.written_length,
            increment = bytes,
            "GrowSegment: updating written length"
        );
        self.written_length = self.written_length.saturating_add(bytes);
    }

    /// Attempts to update the hash with the given data.
    ///
    /// Always returns `false` because hash computation is not supported for
    /// grow segments. The total size is unknown, so there is no expected hash
    /// to verify against.
    pub fn update_hash(&self, _begin: u64, _data: &[u8]) -> bool {
        false
    }

    /// Returns whether the hash has been calculated.
    ///
    /// Always returns `false` because hash computation is not supported.
    pub fn is_hash_calculated(&self) -> bool {
        false
    }

    /// Returns the hash digest as a hex string.
    ///
    /// Always returns an empty string because hash computation is not
    /// supported for grow segments.
    pub fn digest(&self) -> String {
        String::new()
    }

    /// Resets the segment to its initial state.
    ///
    /// Sets `written_length` back to 0. In the C++ implementation, this also
    /// clears all blocks on the wrapped `Piece`. In this Rust version, the
    /// piece is implicitly cleared since `written_length` returns to 0.
    pub fn clear(&mut self) {
        trace!(previous = self.written_length, "GrowSegment: clearing");
        self.written_length = 0;
    }
}

impl Default for GrowSegment {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_has_zero_written_length() {
        let seg = GrowSegment::new();
        assert_eq!(seg.written_length(), 0);
        assert_eq!(seg.position_to_write(), 0);
    }

    #[test]
    fn test_default_equals_new() {
        let seg_default = GrowSegment::default();
        let seg_new = GrowSegment::new();
        assert_eq!(seg_default, seg_new);
    }

    #[test]
    fn test_never_complete() {
        let mut seg = GrowSegment::new();
        assert!(!seg.is_complete());

        seg.update_written_length(1024);
        assert!(!seg.is_complete());

        seg.update_written_length(u64::MAX - 1024);
        assert!(!seg.is_complete());
    }

    #[test]
    fn test_index_always_zero() {
        let seg = GrowSegment::new();
        assert_eq!(seg.index(), 0);
    }

    #[test]
    fn test_position_always_zero() {
        let mut seg = GrowSegment::new();
        assert_eq!(seg.position(), 0);

        seg.update_written_length(9999);
        assert_eq!(seg.position(), 0);
    }

    #[test]
    fn test_length_always_zero() {
        let mut seg = GrowSegment::new();
        assert_eq!(seg.length(), 0);
        assert_eq!(seg.segment_length(), 0);

        seg.update_written_length(5000);
        assert_eq!(seg.length(), 0);
        assert_eq!(seg.segment_length(), 0);
    }

    #[test]
    fn test_position_to_write_equals_written_length() {
        let mut seg = GrowSegment::new();
        assert_eq!(seg.position_to_write(), 0);

        seg.update_written_length(100);
        assert_eq!(seg.position_to_write(), 100);
        assert_eq!(seg.position_to_write(), seg.written_length());

        seg.update_written_length(200);
        assert_eq!(seg.position_to_write(), 300);
        assert_eq!(seg.position_to_write(), seg.written_length());
    }

    #[test]
    fn test_update_written_length_accumulates() {
        let mut seg = GrowSegment::new();

        seg.update_written_length(1024);
        assert_eq!(seg.written_length(), 1024);

        seg.update_written_length(2048);
        assert_eq!(seg.written_length(), 3072);

        seg.update_written_length(0);
        assert_eq!(seg.written_length(), 3072);
    }

    #[test]
    fn test_update_written_length_saturating_add() {
        let mut seg = GrowSegment::new();

        // Set written_length near u64::MAX
        seg.update_written_length(u64::MAX - 100);
        assert_eq!(seg.written_length(), u64::MAX - 100);

        // Adding more should saturate at u64::MAX instead of overflowing
        seg.update_written_length(200);
        assert_eq!(seg.written_length(), u64::MAX);
    }

    #[test]
    fn test_update_hash_always_false() {
        let seg = GrowSegment::new();
        assert!(!seg.update_hash(0, &[]));
        assert!(!seg.update_hash(0, &[1, 2, 3, 4]));
        assert!(!seg.update_hash(100, &[0xAA; 1024]));
    }

    #[test]
    fn test_is_hash_calculated_always_false() {
        let seg = GrowSegment::new();
        assert!(!seg.is_hash_calculated());
    }

    #[test]
    fn test_digest_always_empty() {
        let seg = GrowSegment::new();
        assert!(seg.digest().is_empty());
    }

    #[test]
    fn test_clear_resets_written_length() {
        let mut seg = GrowSegment::new();

        seg.update_written_length(4096);
        assert_eq!(seg.written_length(), 4096);

        seg.clear();
        assert_eq!(seg.written_length(), 0);
        assert_eq!(seg.position_to_write(), 0);
    }

    #[test]
    fn test_clear_then_update() {
        let mut seg = GrowSegment::new();

        seg.update_written_length(1000);
        seg.clear();
        seg.update_written_length(500);

        assert_eq!(seg.written_length(), 500);
        assert!(!seg.is_complete());
    }

    #[test]
    fn test_clone_independence() {
        let mut seg = GrowSegment::new();
        seg.update_written_length(2048);

        let clone = seg.clone();
        assert_eq!(clone.written_length(), 2048);

        // Mutating the original does not affect the clone
        seg.update_written_length(1024);
        assert_eq!(seg.written_length(), 3072);
        assert_eq!(clone.written_length(), 2048);
    }

    #[test]
    fn test_equality() {
        let mut seg1 = GrowSegment::new();
        let mut seg2 = GrowSegment::new();

        assert_eq!(seg1, seg2);

        seg1.update_written_length(1024);
        assert_ne!(seg1, seg2);

        seg2.update_written_length(1024);
        assert_eq!(seg1, seg2);
    }

    #[test]
    fn test_large_written_length() {
        let mut seg = GrowSegment::new();

        // Simulate a large download (4 GB)
        seg.update_written_length(4 * 1024 * 1024 * 1024);
        assert_eq!(seg.written_length(), 4 * 1024 * 1024 * 1024);
        assert_eq!(seg.position_to_write(), 4 * 1024 * 1024 * 1024);
        assert!(!seg.is_complete());
        assert_eq!(seg.length(), 0);
    }

    #[test]
    fn test_incremental_download_simulation() {
        // Simulate a chunked download with incremental updates
        let mut seg = GrowSegment::new();
        let chunk_sizes: &[u64] = &[8192, 16384, 32768, 65536, 131072];

        let mut total = 0u64;
        for &chunk in chunk_sizes {
            seg.update_written_length(chunk);
            total += chunk;
            assert_eq!(seg.written_length(), total);
            assert_eq!(seg.position_to_write(), total);
            assert!(!seg.is_complete());
        }

        assert_eq!(seg.written_length(), 253952);
    }

    #[test]
    fn test_clear_multiple_times() {
        let mut seg = GrowSegment::new();

        seg.update_written_length(100);
        seg.clear();
        assert_eq!(seg.written_length(), 0);

        seg.clear(); // Double clear should be safe
        assert_eq!(seg.written_length(), 0);

        seg.update_written_length(200);
        assert_eq!(seg.written_length(), 200);
    }
}
