//! Pieced segment for fixed-length piece downloads.
//!
//! `PiecedSegment` wraps a [`Piece`] and exposes the C++ `Segment` interface
//! for downloads where the total content length is known in advance. This is
//! the Rust equivalent of the C++ `PiecedSegment` class.
//!
//! # Key Semantics
//!
//! - `position()` = `index * piece_length` (byte offset in the file)
//! - `position_to_write()` = `position() + written_length`
//! - `length()` = `piece.length()` (actual bytes, may be less than piece_length for the last piece)
//! - `segment_length()` = `piece_length` (nominal piece length)
//! - `is_complete()` delegates to `piece.is_complete()`
//! - `update_written_length()` marks blocks as completed on the underlying piece
//!
//! # Ownership
//!
//! The `PiecedSegment` owns its `Piece` directly. When the segment is
//! checked out from `SegmentMan`, ownership of the `Piece` transfers to
//! the segment. When the segment is cancelled or completed, the piece
//! is returned to `PieceStorage`.

use super::piece::Piece;
use tracing::trace;

/// Segment for downloads where the total size is known.
///
/// Wraps a `Piece` with a fixed `piece_length`, providing the standard
/// segment interface for position/length/written-length tracking and
/// hash verification.
pub struct PiecedSegment {
    /// The underlying piece being downloaded
    piece: Piece,
    /// Nominal piece length in bytes (may differ from piece.length()
    /// for the last piece of the file)
    piece_length: u64,
    /// Bytes written so far in this segment
    written_length: u64,
}

impl PiecedSegment {
    /// Creates a new `PiecedSegment` wrapping the given piece.
    ///
    /// # Arguments
    ///
    /// * `piece_length` - The nominal piece length (from download context)
    /// * `piece` - The piece to wrap; ownership transfers to this segment
    pub fn new(piece_length: u64, piece: Piece) -> Self {
        let initial_written = piece.completed_length();
        trace!(
            index = piece.index(),
            length = piece.length(),
            piece_length,
            initial_written,
            "PiecedSegment: created"
        );
        PiecedSegment {
            piece,
            piece_length,
            written_length: initial_written,
        }
    }

    /// Returns `true` if all blocks of the underlying piece are completed.
    pub fn is_complete(&self) -> bool {
        self.piece.is_complete()
    }

    /// Returns the piece index.
    pub fn index(&self) -> usize {
        self.piece.index()
    }

    /// Returns the byte offset of this segment in the file.
    ///
    /// Computed as `index * piece_length`.
    pub fn position(&self) -> u64 {
        self.piece.index() as u64 * self.piece_length
    }

    /// Returns the next byte position to write to.
    ///
    /// Computed as `position() + written_length`.
    pub fn position_to_write(&self) -> u64 {
        self.position() + self.written_length
    }

    /// Returns the actual length of this segment in bytes.
    ///
    /// This may be less than `segment_length()` for the last piece of a file.
    pub fn length(&self) -> u64 {
        self.piece.length()
    }

    /// Returns the nominal piece length.
    ///
    /// This is the standard piece length from the download context,
    /// which may be larger than `length()` for the last piece.
    pub fn segment_length(&self) -> u64 {
        self.piece_length
    }

    /// Returns how many bytes have been written so far.
    pub fn written_length(&self) -> u64 {
        self.written_length
    }

    /// Increments the written length by `bytes` and marks the corresponding
    /// blocks as completed on the underlying piece.
    ///
    /// In the C++ implementation, this method marks blocks as completed
    /// one at a time using `Piece::completeBlock()` for each block that
    /// falls within the newly written range.
    pub fn update_written_length(&mut self, bytes: u64) {
        if bytes == 0 {
            return;
        }

        let old_pos = self.written_length;
        let new_pos = self.written_length + bytes;

        // Mark blocks as completed on the piece
        let block_length = self.piece.block_length() as u64;
        if block_length > 0 {
            let start_block = (old_pos / block_length) as usize;
            let end_block = ((new_pos + block_length - 1) / block_length) as usize;
            let num_blocks = self.piece.count_blocks();

            for block_idx in start_block..std::cmp::min(end_block, num_blocks) {
                // Check if the block is fully covered by the new written range
                let block_start = block_idx as u64 * block_length;
                let block_end = std::cmp::min(block_start + block_length, self.piece.length());
                if new_pos >= block_end {
                    self.piece.complete_block(block_idx);
                }
            }
        }

        self.written_length = new_pos;
        trace!(
            index = self.piece.index(),
            old_written = old_pos,
            increment = bytes,
            new_written = self.written_length,
            "PiecedSegment: updated written length"
        );
    }

    /// Updates the hash computation with data at the given offset.
    ///
    /// Delegates to `Piece::update_hash`.
    pub fn update_hash(&mut self, begin: u64, data: &[u8]) -> bool {
        self.piece.update_hash(begin, data)
    }

    /// Returns `true` if the hash has been fully computed.
    pub fn is_hash_calculated(&self) -> bool {
        self.piece.is_hash_calculated()
    }

    /// Returns the hash digest as a hex string.
    ///
    /// Returns an empty string if no hash is available.
    pub fn digest(&mut self) -> String {
        match self.piece.get_digest() {
            Some(bytes) => hex::encode(bytes),
            None => String::new(),
        }
    }

    /// Returns a reference to the underlying piece.
    pub fn piece(&self) -> &Piece {
        &self.piece
    }

    /// Returns a mutable reference to the underlying piece.
    pub fn piece_mut(&mut self) -> &mut Piece {
        &mut self.piece
    }
}

impl std::fmt::Debug for PiecedSegment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiecedSegment")
            .field("index", &self.piece.index())
            .field("piece_length", &self.piece_length)
            .field("written_length", &self.written_length)
            .field("piece", &self.piece)
            .finish()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_with_piece() {
        let piece = Piece::new(3, 65536);
        let seg = PiecedSegment::new(65536, piece);
        assert_eq!(seg.index(), 3);
        assert_eq!(seg.position(), 3 * 65536);
        assert_eq!(seg.position_to_write(), 3 * 65536);
        assert_eq!(seg.length(), 65536);
        assert_eq!(seg.segment_length(), 65536);
        assert_eq!(seg.written_length(), 0);
        assert!(!seg.is_complete());
    }

    #[test]
    fn test_last_piece_shorter_length() {
        // Total length = 100000, piece_length = 65536
        // Last piece: index=1, length=100000 - 65536 = 34464
        let piece = Piece::new(1, 34464);
        let seg = PiecedSegment::new(65536, piece);
        assert_eq!(seg.index(), 1);
        assert_eq!(seg.position(), 65536);
        assert_eq!(seg.length(), 34464);
        assert_eq!(seg.segment_length(), 65536);
    }

    #[test]
    fn test_update_written_length_single_block() {
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);
        assert_eq!(seg.written_length(), 0);

        // Write one full block (16384 bytes)
        seg.update_written_length(16384);
        assert_eq!(seg.written_length(), 16384);
        assert!(seg.piece().has_block(0));
    }

    #[test]
    fn test_update_written_length_multiple_blocks() {
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);

        // Write two blocks
        seg.update_written_length(32768);
        assert_eq!(seg.written_length(), 32768);
        assert!(seg.piece().has_block(0));
        assert!(seg.piece().has_block(1));
        assert!(!seg.piece().has_block(2));
    }

    #[test]
    fn test_update_written_length_complete() {
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);

        seg.update_written_length(65536);
        assert_eq!(seg.written_length(), 65536);
        assert!(seg.is_complete());
    }

    #[test]
    fn test_update_written_length_incremental() {
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);

        seg.update_written_length(16384);
        assert_eq!(seg.written_length(), 16384);
        assert_eq!(seg.position_to_write(), 16384);

        seg.update_written_length(16384);
        assert_eq!(seg.written_length(), 32768);
        assert_eq!(seg.position_to_write(), 32768);
    }

    #[test]
    fn test_update_written_length_zero_is_noop() {
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);

        seg.update_written_length(0);
        assert_eq!(seg.written_length(), 0);
    }

    #[test]
    fn test_hash_update_and_digest() {
        let piece = Piece::new(0, 4);
        let mut seg = PiecedSegment::new(4, piece);
        seg.piece_mut().set_hash_type("sha-1");

        assert!(seg.update_hash(0, b"test"));
        assert!(seg.is_hash_calculated());

        let digest = seg.digest();
        assert!(!digest.is_empty());
        // SHA1 of "test" = a94a8fe5ccb19ba61c4c0873d391e987982fbbd3
        assert_eq!(digest.len(), 40); // 20 bytes = 40 hex chars
    }

    #[test]
    fn test_digest_empty_when_no_hash() {
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);
        assert!(seg.digest().is_empty());
    }

    #[test]
    fn test_piece_accessors() {
        let piece = Piece::new(5, 32768);
        let mut seg = PiecedSegment::new(65536, piece);

        // Read access
        assert_eq!(seg.piece().index(), 5);
        assert_eq!(seg.piece().length(), 32768);

        // Mutable access
        seg.piece_mut().set_hash_type("sha-256");
        assert_eq!(seg.piece().hash_type(), Some("sha-256"));
    }

    #[test]
    fn test_debug_format() {
        let piece = Piece::new(2, 65536);
        let seg = PiecedSegment::new(65536, piece);
        let debug_str = format!("{:?}", seg);
        assert!(debug_str.contains("index: 2"));
        assert!(debug_str.contains("piece_length: 65536"));
    }

    #[test]
    fn test_position_calculation() {
        // Piece at index 5 with piece_length 1MB
        let piece = Piece::new(5, 1048576);
        let seg = PiecedSegment::new(1048576, piece);
        assert_eq!(seg.position(), 5 * 1048576);
        assert_eq!(seg.position_to_write(), 5 * 1048576); // no bytes written yet
    }

    #[test]
    fn test_update_written_length_partial_block() {
        // Write less than a full block — the block should NOT be marked complete
        let piece = Piece::new(0, 65536);
        let mut seg = PiecedSegment::new(65536, piece);

        // Write 100 bytes (less than one block of 16384)
        seg.update_written_length(100);
        assert_eq!(seg.written_length(), 100);
        // Block 0 spans 0..16384, so 100 bytes doesn't complete it
        assert!(!seg.piece().has_block(0));
    }
}
