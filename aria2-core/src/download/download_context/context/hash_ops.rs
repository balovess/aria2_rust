//! Piece hash and whole-file checksum operations.

use tracing::debug;

use super::struct_def::EMPTY_STRING;
use crate::download::download_context::DownloadContext;

impl DownloadContext {
    // -----------------------------------------------------------------------
    // Piece Info
    // -----------------------------------------------------------------------

    /// Return the piece length in bytes (0 = unknown).
    pub fn get_piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Set the piece length in bytes.
    pub fn set_piece_length(&mut self, length: u32) {
        self.piece_length = length;
        debug!(piece_length = length, "Piece length updated");
    }

    /// Calculate the number of pieces.
    ///
    /// Returns `(last_offset + piece_length - 1) / piece_length`, or 0 if
    /// `piece_length` is 0 or there are no file entries.
    pub fn get_num_pieces(&self) -> usize {
        if self.piece_length == 0 || self.file_entries.is_empty() {
            return 0;
        }
        let last_entry = self
            .file_entries
            .last()
            .expect("get_num_pieces: file_entries is non-empty but last() returned None");
        let last_offset = last_entry.last_offset();
        last_offset.div_ceil(self.piece_length as u64) as usize
    }

    // -----------------------------------------------------------------------
    // Piece Hash Access
    // -----------------------------------------------------------------------

    /// Return the piece hash at the given index, or an empty string if
    /// out of bounds.
    ///
    /// Matches C++ `getPieceHash` which returns `A2STR::NIL` for invalid
    /// indices. We return a static `&str` to avoid allocation.
    pub fn get_piece_hash(&self, index: usize) -> &str {
        self.piece_hashes
            .get(index)
            .map(|s| s.as_str())
            .unwrap_or(EMPTY_STRING)
    }

    /// Return a reference to all piece hashes.
    pub fn get_piece_hashes(&self) -> &[String] {
        &self.piece_hashes
    }

    /// Return the hash algorithm used for piece hashes (e.g. "sha-1").
    pub fn get_piece_hash_type(&self) -> &str {
        &self.piece_hash_type
    }

    /// Set piece hashes and their algorithm.
    ///
    /// Replaces any existing piece hashes.
    pub fn set_piece_hashes(&mut self, hash_type: String, hashes: Vec<String>) {
        self.piece_hash_type = hash_type;
        self.piece_hashes = hashes;
        debug!(
            hash_type = %self.piece_hash_type,
            count = self.piece_hashes.len(),
            "Piece hashes set"
        );
    }

    // -----------------------------------------------------------------------
    // Whole-file Checksum
    // -----------------------------------------------------------------------

    /// Return the whole-file hash digest value.
    pub fn get_digest(&self) -> &str {
        &self.digest
    }

    /// Return the whole-file hash algorithm name.
    pub fn get_hash_type(&self) -> &str {
        &self.hash_type
    }

    /// Set the whole-file checksum.
    pub fn set_digest(&mut self, hash_type: String, digest: String) {
        self.hash_type = hash_type;
        self.digest = digest;
        debug!(hash_type = %self.hash_type, "Whole-file checksum set");
    }

    /// Whether a whole-file checksum verification is needed.
    ///
    /// Returns `true` when:
    /// - No piece hash type is set (piece-level verification won't happen), AND
    /// - Both digest and hash_type are present, AND
    /// - The checksum has NOT been verified yet.
    ///
    /// This matches the C++ `isChecksumVerificationNeeded()` logic.
    pub fn is_checksum_verification_needed(&self) -> bool {
        self.piece_hash_type.is_empty()
            && !self.digest.is_empty()
            && !self.hash_type.is_empty()
            && !self.checksum_verified
    }

    /// Whether a whole-file checksum is available (digest + hash_type present).
    pub fn is_checksum_verification_available(&self) -> bool {
        !self.digest.is_empty() && !self.hash_type.is_empty()
    }

    /// Whether piece hash verification is available.
    ///
    /// Returns `true` when:
    /// - `piece_hash_type` is non-empty, AND
    /// - At least one piece hash exists, AND
    /// - The number of piece hashes equals `get_num_pieces()`.
    pub fn is_piece_hash_verification_available(&self) -> bool {
        !self.piece_hash_type.is_empty()
            && !self.piece_hashes.is_empty()
            && self.piece_hashes.len() == self.get_num_pieces()
    }

    /// Whether a whole-file checksum verification is pending (aria2-next).
    ///
    /// Stricter than `is_checksum_verification_needed()`: returns true whenever
    /// a whole-file hash is available and NOT verified, regardless of whether
    /// piece hash verification is also available.
    pub fn is_checksum_verification_pending(&self) -> bool {
        self.is_checksum_verification_available() && !self.checksum_verified
    }

    /// Set whether the checksum has been verified.
    pub fn set_checksum_verified(&mut self, verified: bool) {
        self.checksum_verified = verified;
        debug!(verified, "Checksum verified flag updated");
    }
}
