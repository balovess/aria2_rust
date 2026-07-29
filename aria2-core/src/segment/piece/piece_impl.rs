//! Piece struct definition and core implementation.

use tracing::trace;

use super::bitfield::BlockBitfield;
use super::completion::{finalize_hash, HashState};

/// Default block length: 16 KiB (16384 bytes), matching aria2 C++ BLOCK_LENGTH.
pub const DEFAULT_BLOCK_LENGTH: u32 = 16 * 1024;

/// A piece of a download, tracking block-level completion with a dual-bitfield
/// (completed + in-use), user reference counting, and hash verification.
///
/// This is the Rust equivalent of the C++ aria2 `Piece` class.
///
/// # Block Tracking
///
/// Each piece is divided into fixed-size blocks (typically 16 KiB). Two bitfields
/// track block state:
/// - **completed**: blocks that have been fully downloaded
/// - **in_use**: blocks currently being requested by a peer/connection
///
/// A "missing unused" block is one that is neither completed nor in-use.
///
/// # User Tracking
///
/// Multiple connections/commands can reference the same piece concurrently.
/// The `users` vector tracks CUIDs to prevent premature piece cleanup.
///
/// # Hash Verification
///
/// Supports incremental hash computation during download. Hash data must be
/// fed sequentially (in byte offset order) for correctness.
///
/// # Examples
///
/// ```
/// use aria2_core::segment::piece::{Piece, DEFAULT_BLOCK_LENGTH};
///
/// // Create a piece with index 0, length 65536 bytes (4 blocks of 16 KiB)
/// let mut piece = Piece::new(0, 65536);
/// assert_eq!(piece.count_blocks(), 4);
/// assert!(!piece.is_complete());
///
/// // Mark block 0 as in-use (requested by a peer)
/// let block_idx = piece.get_missing_unused_block_index().unwrap();
/// assert_eq!(block_idx, 0);
/// assert!(piece.is_block_in_use(0));
///
/// // Complete block 0
/// piece.complete_block(0);
/// assert!(piece.has_block(0));
/// assert!(!piece.is_block_in_use(0)); // no longer in-use after completion
/// ```
pub struct Piece {
    /// Bitfield tracking completed (downloaded) blocks
    pub(super) completed: BlockBitfield,
    /// Bitfield tracking blocks currently in-use (being requested)
    pub(super) in_use: BlockBitfield,
    /// CUIDs of users (peers/commands) currently referencing this piece
    pub(super) users: Vec<u64>,
    /// Hash algorithm name (e.g., "sha-1", "sha-256")
    pub(super) hash_type: Option<String>,
    /// Incremental hash context for piece verification (lazily initialized)
    pub(super) hash_state: Option<HashState>,
    /// Next expected byte offset for sequential hash update
    pub(super) next_begin: u64,
    /// Piece index in the download
    pub(super) index: usize,
    /// Total length of this piece in bytes
    pub(super) length: u64,
    /// Block length in bytes (typically 16 KiB)
    pub(super) block_length: u32,
    /// Whether this piece is currently used by a segment
    pub(super) used_by_segment: bool,
    // TODO: WrDiskCache support (wr_cache field)
}

impl Piece {
    /// Creates a new piece with the given index, length, and default block
    /// length (16 KiB).
    pub fn new(index: usize, length: u64) -> Self {
        Self::with_block_length(index, length, DEFAULT_BLOCK_LENGTH)
    }

    /// Creates a new piece with the given index, length, and custom block
    /// length.
    pub fn with_block_length(index: usize, length: u64, block_length: u32) -> Self {
        let num_blocks = Self::compute_num_blocks(length, block_length);
        Piece {
            completed: BlockBitfield::new(num_blocks),
            in_use: BlockBitfield::new(num_blocks),
            users: Vec::new(),
            hash_type: None,
            hash_state: None,
            next_begin: 0,
            index,
            length,
            block_length,
            used_by_segment: false,
        }
    }

    /// Computes the number of blocks for a given piece length and block length.
    #[inline]
    fn compute_num_blocks(length: u64, block_length: u32) -> usize {
        if length == 0 || block_length == 0 {
            0
        } else {
            length.div_ceil(block_length as u64) as usize
        }
    }

    /// Returns the length of the last block (may be shorter than block_length).
    fn last_block_length(&self) -> u32 {
        if self.length == 0 || self.block_length == 0 {
            return 0;
        }
        let remainder = self.length % self.block_length as u64;
        if remainder == 0 {
            self.block_length
        } else {
            remainder as u32
        }
    }

    // -- Accessors ----------------------------------------------------------

    /// Returns the piece index.
    #[inline]
    pub fn index(&self) -> usize {
        self.index
    }

    /// Sets the piece index.
    #[inline]
    pub fn set_index(&mut self, index: usize) {
        self.index = index;
    }

    /// Returns the total length of this piece in bytes.
    #[inline]
    pub fn length(&self) -> u64 {
        self.length
    }

    /// Sets the total length of this piece.
    #[inline]
    pub fn set_length(&mut self, length: u64) {
        self.length = length;
    }

    /// Returns the block length in bytes.
    #[inline]
    pub fn block_length(&self) -> u32 {
        self.block_length
    }

    // -- Block queries ------------------------------------------------------

    /// Returns the total number of blocks in this piece.
    pub fn count_blocks(&self) -> usize {
        self.completed.len()
    }

    /// Returns the length of a specific block.
    ///
    /// For all blocks except the last, this is `block_length`.
    /// For the last block, it may be shorter if the piece length is not
    /// a multiple of the block length.
    ///
    /// Returns 0 if the block index is out of range.
    pub fn block_length_at(&self, block_index: usize) -> u32 {
        let num_blocks = self.count_blocks();
        if block_index >= num_blocks {
            return 0;
        }
        if num_blocks > 0 && block_index == num_blocks - 1 {
            self.last_block_length()
        } else {
            self.block_length
        }
    }

    /// Returns the default block length (same as `block_length()`).
    #[inline]
    pub fn default_block_length(&self) -> u32 {
        self.block_length
    }

    /// Returns the number of completed (downloaded) blocks.
    pub fn count_completed_blocks(&self) -> usize {
        self.completed.count_set()
    }

    /// Returns the number of missing (not yet completed) blocks.
    pub fn count_missing_blocks(&self) -> usize {
        self.completed.count_clear()
    }

    /// Returns true if the given block is completed.
    pub fn has_block(&self, block_index: usize) -> bool {
        self.completed.test(block_index)
    }

    /// Returns true if all blocks of this piece have been downloaded.
    pub fn is_complete(&self) -> bool {
        self.completed.is_all_set()
    }

    /// Returns the raw bytes of the completed bitfield.
    pub fn completed_bitfield_bytes(&self) -> &[u8] {
        self.completed.as_bytes()
    }

    /// Returns the byte length of the completed bitfield storage.
    pub fn completed_bitfield_byte_len(&self) -> usize {
        self.completed.as_bytes().len()
    }

    /// Sets the completed bitfield from raw bytes.
    ///
    /// The bit count is preserved from construction; `data` is interpreted
    /// as the new completed-state bitfield.
    pub fn set_completed_bitfield(&mut self, data: &[u8]) {
        self.completed = BlockBitfield::from_bytes(data, self.count_blocks());
    }

    /// Returns the total completed length in bytes.
    ///
    /// Accounts for the last block potentially being shorter than
    /// `block_length`.
    pub fn completed_length(&self) -> u64 {
        let num_blocks = self.count_blocks();
        if num_blocks == 0 {
            return 0;
        }
        let completed_count = self.completed.count_set();
        if completed_count == 0 {
            return 0;
        }
        let last_block_idx = num_blocks - 1;
        if self.completed.test(last_block_idx) {
            // Last block is completed - use its actual (possibly shorter) length
            (completed_count - 1) as u64 * self.block_length as u64
                + self.last_block_length() as u64
        } else {
            // Last block is not completed - all completed blocks use full block_length
            completed_count as u64 * self.block_length as u64
        }
    }

    // -- Block mutations ----------------------------------------------------

    /// Finds the first missing unused block, marks it as in-use, and returns
    /// its index.
    ///
    /// A "missing unused" block is one that is neither completed nor in-use.
    /// This method marks the found block as in-use before returning.
    ///
    /// Returns `None` if all blocks are either completed or in-use.
    pub fn get_missing_unused_block_index(&mut self) -> Option<usize> {
        let num_blocks = self.count_blocks();
        for i in 0..num_blocks {
            if !self.completed.test(i) && !self.in_use.test(i) {
                self.in_use.set(i);
                return Some(i);
            }
        }
        None
    }

    /// Finds up to `n` missing unused blocks, marks them as in-use, and
    /// returns their indices.
    ///
    /// A "missing unused" block is one that is neither completed nor in-use.
    /// This method marks all found blocks as in-use before returning.
    pub fn get_missing_unused_block_indexes(&mut self, n: usize) -> Vec<usize> {
        let mut result = Vec::with_capacity(n);
        let num_blocks = self.count_blocks();
        for i in 0..num_blocks {
            if result.len() >= n {
                break;
            }
            if !self.completed.test(i) && !self.in_use.test(i) {
                self.in_use.set(i);
                result.push(i);
            }
        }
        result
    }

    /// Returns the index of the first missing block (completed bitfield not
    /// set).
    ///
    /// Unlike `get_missing_unused_block_index`, this does not modify the
    /// in-use bitfield.
    pub fn get_first_missing_block_index(&self) -> Option<usize> {
        self.completed.find_first_clear()
    }

    /// Returns a bitfield of all missing (not completed) block indexes.
    ///
    /// A set bit in the returned bitfield indicates a missing block.
    /// Returns the raw byte representation since `BlockBitfield` is private.
    pub fn missing_block_bitfield_bytes(&self) -> Vec<u8> {
        let num_blocks = self.count_blocks();
        let mut missing = BlockBitfield::all_set(num_blocks);
        for i in 0..num_blocks {
            if self.completed.test(i) {
                missing.clear(i);
            }
        }
        missing.data.clone()
    }

    /// Marks a block as completed (downloaded) and removes it from in-use.
    pub fn complete_block(&mut self, block_index: usize) {
        self.completed.set(block_index);
        self.in_use.clear(block_index);
    }

    /// Cancels a block request by removing it from in-use.
    ///
    /// This does NOT mark the block as completed; it simply releases the
    /// in-use flag so another peer can request it.
    pub fn cancel_block(&mut self, block_index: usize) {
        self.in_use.clear(block_index);
    }

    /// Clears all blocks (both completed and in-use), resetting the piece.
    ///
    /// Note: WrDiskCache clearing is not yet implemented (TODO).
    pub fn clear_all_blocks(&mut self) {
        self.completed.clear_all();
        self.in_use.clear_all();
        // TODO: Clear WrDiskCache when implemented
    }

    /// Sets a block as in-use (being requested by a peer/connection).
    pub fn set_block_in_use(&mut self, block_index: usize) {
        self.in_use.set(block_index);
    }

    /// Clears a block's in-use flag.
    pub fn clear_block_in_use(&mut self, block_index: usize) {
        self.in_use.clear(block_index);
    }

    /// Returns true if the given block is in-use.
    pub fn is_block_in_use(&self, block_index: usize) -> bool {
        self.in_use.test(block_index)
    }

    /// Sets whether this piece is used by a segment.
    pub fn set_used_by_segment(&mut self, used: bool) {
        self.used_by_segment = used;
    }

    /// Returns true if this piece is used by a segment.
    pub fn is_used_by_segment(&self) -> bool {
        self.used_by_segment
    }

    /// Marks all blocks as completed.
    pub fn set_all_blocks(&mut self) {
        self.completed.set_all();
    }

    /// Reconfigures the piece with a new length, losing all current bitfield
    /// state.
    ///
    /// Uses `i32::MAX` as the block length to minimize block count overhead,
    /// matching the C++ aria2 behavior (currently only used by `GrowSegment`).
    pub fn reconfigure(&mut self, length: u64) {
        self.length = length;
        let max_block_length = i32::MAX as u32;
        let num_blocks = Self::compute_num_blocks(length, max_block_length);
        self.completed = BlockBitfield::new(num_blocks);
        self.in_use = BlockBitfield::new(num_blocks);
        self.block_length = max_block_length;
    }

    // -- User tracking ------------------------------------------------------

    /// Adds a user (CUID) to this piece's user list.
    ///
    /// If the user is already tracked, this is a no-op.
    pub fn add_user(&mut self, cuid: u64) {
        if !self.used_by(cuid) {
            self.users.push(cuid);
        }
    }

    /// Removes a user (CUID) from this piece's user list.
    pub fn remove_user(&mut self, cuid: u64) {
        self.users.retain(|&c| c != cuid);
    }

    /// Returns the number of users currently referencing this piece.
    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    /// Returns true if the given CUID is among this piece's users.
    pub fn used_by(&self, cuid: u64) -> bool {
        self.users.contains(&cuid)
    }

    /// Returns true if this piece has any users.
    pub fn is_used(&self) -> bool {
        !self.users.is_empty()
    }

    // -- Hash verification --------------------------------------------------

    /// Sets the hash algorithm type for piece verification.
    ///
    /// Supported values: "sha-1", "sha1", "sha-256", "sha256", "sha-512",
    /// "sha512", "md5". Case-insensitive.
    ///
    /// This destroys any existing hash context.
    pub fn set_hash_type(&mut self, hash_type: &str) {
        self.hash_type = Some(hash_type.to_string());
        self.destroy_hash_context();
    }

    /// Returns the hash type name, if set.
    pub fn hash_type(&self) -> Option<&str> {
        self.hash_type.as_deref()
    }

    /// Updates the hash computation with data at the given offset.
    ///
    /// Data must be fed sequentially starting from offset 0. Only when `begin`
    /// equals the internal `next_begin` counter will the hash be updated.
    /// This ensures hash computation proceeds in order.
    ///
    /// Returns `true` if the hash was updated, `false` if the offset didn't
    /// match or would exceed the piece length.
    pub fn update_hash(&mut self, begin: u64, data: &[u8]) -> bool {
        let hash_type = match &self.hash_type {
            Some(ht) => ht,
            None => return false,
        };

        if begin != self.next_begin {
            trace!(
                "Piece::update_hash: offset mismatch, expected={}, got={}, piece={}",
                self.next_begin, begin, self.index
            );
            return false;
        }

        if self.next_begin + data.len() as u64 > self.length {
            trace!(
                "Piece::update_hash: data would exceed piece length,                  next_begin={} + data_len={} > length={}, piece={}",
                self.next_begin,
                data.len(),
                self.length,
                self.index
            );
            return false;
        }

        // Lazily create hash context on first update
        if self.hash_state.is_none() {
            match HashState::new(hash_type) {
                Some(state) => self.hash_state = Some(state),
                None => return false,
            }
        }

        if let Some(ref mut state) = self.hash_state {
            state.update(data);
            self.next_begin += data.len() as u64;
            true
        } else {
            false
        }
    }

    /// Returns true if the hash has been fully computed (all piece data fed
    /// sequentially).
    pub fn is_hash_calculated(&self) -> bool {
        self.hash_state.is_some() && self.next_begin == self.length
    }

    /// Returns the raw hash digest bytes.
    ///
    /// This method consumes the hash context. Subsequent calls without
    /// `update_hash` will return `None`. This matches the C++ behavior
    /// where `getDigest()` returns the hash value only once.
    pub fn get_digest(&mut self) -> Option<Vec<u8>> {
        let state = self.hash_state.take()?;
        let digest = finalize_hash(state);
        self.next_begin = 0;
        Some(digest)
    }

    /// Destroys the hash context, resetting the incremental hash state.
    pub fn destroy_hash_context(&mut self) {
        self.hash_state = None;
        self.next_begin = 0;
    }
}
