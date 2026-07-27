//! Piece tracking for segmented downloads.
//!
//! Implements the aria2-compatible `Piece` struct for block-level completion
//! tracking with dual bitfields (completed + in-use), user reference counting,
//! and hash verification.
//!
//! This is the Rust equivalent of the C++ aria2 `Piece` class, adapted to Rust's
//! ownership model. Key differences from the C++ version:
//! - `get_missing_unused_block_index` takes `&mut self` instead of `const` + `mutable`
//! - Hash context uses an enum for static dispatch instead of runtime polymorphism
//! - WrDiskCache support is TODO (will be added when the cache module is implemented)
//! - Uses a self-contained bitfield instead of the aria2-protocol Bitfield

use digest::Digest;
use tracing::trace;

/// Default block length: 16 KiB (16384 bytes), matching aria2 C++ BLOCK_LENGTH.
pub const DEFAULT_BLOCK_LENGTH: u32 = 16 * 1024;

// ── Self-contained bitfield ─────────────────────────────────────────────────

/// A simple bitfield for tracking block completion/in-use status.
///
/// Uses MSB-first bit ordering (bit 0 is the MSB of byte 0), matching C++ aria2.
#[derive(Clone, Debug)]
struct BlockBitfield {
    data: Vec<u8>,
    num_bits: usize,
}

impl BlockBitfield {
    fn new(num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        BlockBitfield {
            data: vec![0u8; num_bytes],
            num_bits,
        }
    }

    fn test(&self, index: usize) -> bool {
        if index >= self.num_bits {
            return false;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        (self.data[byte] & (1 << bit)) != 0
    }

    fn set(&mut self, index: usize) {
        if index >= self.num_bits {
            return;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        self.data[byte] |= 1 << bit;
    }

    fn unset(&mut self, index: usize) {
        if index >= self.num_bits {
            return;
        }
        let byte = index / 8;
        let bit = 7 - (index % 8);
        self.data[byte] &= !(1 << bit);
    }

    fn len(&self) -> usize {
        self.num_bits
    }

    fn count_set(&self) -> usize {
        self.data
            .iter()
            .map(|b| b.count_ones() as usize)
            .sum::<usize>()
            - if !self.num_bits.is_multiple_of(8) {
                // Count extra bits in the last byte that are beyond num_bits
                let extra = 8 - (self.num_bits % 8);
                let last = *self.data.last().unwrap_or(&0);
                let mask = (1u8 << extra) - 1;
                (last & mask).count_ones() as usize
            } else {
                0
            }
    }

    fn count_clear(&self) -> usize {
        self.num_bits.saturating_sub(self.count_set())
    }

    /// Set all bits
    fn set_all(&mut self) {
        for byte in &mut self.data {
            *byte = 0xFF;
        }
        // Clear trailing bits beyond num_bits
        if !self.num_bits.is_multiple_of(8) {
            let extra = 8 - (self.num_bits % 8);
            if let Some(last) = self.data.last_mut() {
                *last &= !((1u8 << extra) - 1);
            }
        }
    }

    /// Clear a bit at index
    fn clear(&mut self, index: usize) {
        self.unset(index);
    }

    /// Find the first clear (unset) bit, returns None if all are set
    fn find_first_clear(&self) -> Option<usize> {
        (0..self.num_bits).find(|&i| !self.test(i))
    }

    /// Create from existing byte data
    fn from_bytes(data: &[u8], num_bits: usize) -> Self {
        let num_bytes = num_bits.div_ceil(8);
        let mut bf = BlockBitfield {
            data: vec![0u8; num_bytes],
            num_bits,
        };
        let copy_len = std::cmp::min(data.len(), num_bytes);
        bf.data[..copy_len].copy_from_slice(&data[..copy_len]);
        bf
    }

    /// Create with all bits set
    fn all_set(num_bits: usize) -> Self {
        let mut bf = BlockBitfield::new(num_bits);
        bf.set_all();
        bf
    }

    /// Returns true if all bits are set
    fn is_all_set(&self) -> bool {
        self.count_set() == self.num_bits
    }

    /// Returns the raw byte slice
    fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Clear all bits
    fn clear_all(&mut self) {
        for byte in &mut self.data {
            *byte = 0;
        }
    }
}

// ── Hash state ─────────────────────────────────────────────────────────────

/// Supported hash algorithms for piece verification.
///
/// Uses static dispatch (enum) instead of dynamic dispatch (`Box<dyn DynDigest>`)
/// for zero-overhead algorithm selection and no `alloc` feature dependency.
enum HashState {
    Sha1(sha1::Sha1),
    Sha256(sha2::Sha256),
    Sha512(sha2::Sha512),
    Md5(md5::Md5),
}

impl HashState {
    /// Creates a new hash state from a hash type name.
    ///
    /// Supports common names: "sha-1", "sha1", "sha-256", "sha256", "sha-512",
    /// "sha512", "md5". Case-insensitive.
    fn new(hash_type: &str) -> Option<Self> {
        match hash_type.to_lowercase().as_str() {
            "sha-1" | "sha1" => Some(HashState::Sha1(sha1::Sha1::new())),
            "sha-256" | "sha256" => Some(HashState::Sha256(sha2::Sha256::new())),
            "sha-512" | "sha512" => Some(HashState::Sha512(sha2::Sha512::new())),
            "md5" => Some(HashState::Md5(md5::Md5::new())),
            other => {
                trace!("Unsupported hash type for piece verification: {}", other);
                None
            }
        }
    }

    /// Feeds data into the hash computation.
    fn update(&mut self, data: &[u8]) {
        match self {
            HashState::Sha1(ctx) => Digest::update(ctx, data),
            HashState::Sha256(ctx) => Digest::update(ctx, data),
            HashState::Sha512(ctx) => Digest::update(ctx, data),
            HashState::Md5(ctx) => md5::Digest::update(ctx, data),
        }
    }

    /// Returns the output size in bytes for the hash algorithm.
    #[allow(dead_code)]
    fn output_size(&self) -> usize {
        match self {
            HashState::Sha1(_) => 20,
            HashState::Sha256(_) => 32,
            HashState::Sha512(_) => 64,
            HashState::Md5(_) => 16,
        }
    }
}

/// Finalizes the hash computation, consuming the state and returning the raw
/// hash bytes.
fn finalize_hash(state: HashState) -> Vec<u8> {
    match state {
        HashState::Sha1(ctx) => ctx.finalize().to_vec(),
        HashState::Sha256(ctx) => ctx.finalize().to_vec(),
        HashState::Sha512(ctx) => ctx.finalize().to_vec(),
        HashState::Md5(ctx) => md5::Digest::finalize(ctx).to_vec(),
    }
}

// ── Piece ──────────────────────────────────────────────────────────────────

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
    completed: BlockBitfield,
    /// Bitfield tracking blocks currently in-use (being requested)
    in_use: BlockBitfield,
    /// CUIDs of users (peers/commands) currently referencing this piece
    users: Vec<u64>,
    /// Hash algorithm name (e.g., "sha-1", "sha-256")
    hash_type: Option<String>,
    /// Incremental hash context for piece verification (lazily initialized)
    hash_state: Option<HashState>,
    /// Next expected byte offset for sequential hash update
    next_begin: u64,
    /// Piece index in the download
    index: usize,
    /// Total length of this piece in bytes
    length: u64,
    /// Block length in bytes (typically 16 KiB)
    block_length: u32,
    /// Whether this piece is currently used by a segment
    used_by_segment: bool,
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

    // ── Accessors ───────────────────────────────────────────────────────

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

    // ── Block queries ───────────────────────────────────────────────────

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

    // ── Block mutations ─────────────────────────────────────────────────

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

    // ── User tracking ───────────────────────────────────────────────────

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

    // ── Hash verification ───────────────────────────────────────────────

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
                "Piece::update_hash: data would exceed piece length, \
                 next_begin={} + data_len={} > length={}, piece={}",
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

// ── Trait implementations ──────────────────────────────────────────────────

impl Clone for Piece {
    fn clone(&self) -> Self {
        Piece {
            completed: self.completed.clone(),
            in_use: self.in_use.clone(),
            users: self.users.clone(),
            hash_type: self.hash_type.clone(),
            hash_state: None, // Hash state is not cloned; it will be re-initialized if needed
            next_begin: 0,
            index: self.index,
            length: self.length,
            block_length: self.block_length,
            used_by_segment: self.used_by_segment,
        }
    }
}

impl PartialEq for Piece {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for Piece {}

impl PartialOrd for Piece {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Piece {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.index.cmp(&other.index)
    }
}

impl std::fmt::Debug for Piece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Piece")
            .field("index", &self.index)
            .field("length", &self.length)
            .field("block_length", &self.block_length)
            .field("num_blocks", &self.count_blocks())
            .field("completed_blocks", &self.count_completed_blocks())
            .field("missing_blocks", &self.count_missing_blocks())
            .field("users", &self.users.len())
            .field("used_by_segment", &self.used_by_segment)
            .field("hash_type", &self.hash_type)
            .field("hash_calculated", &self.is_hash_calculated())
            .finish()
    }
}

impl std::fmt::Display for Piece {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "piece: index={}, length={}", self.index, self.length)
    }
}

impl Default for Piece {
    fn default() -> Self {
        Piece {
            completed: BlockBitfield::new(0),
            in_use: BlockBitfield::new(0),
            users: Vec::new(),
            hash_type: None,
            hash_state: None,
            next_begin: 0,
            index: 0,
            length: 0,
            block_length: DEFAULT_BLOCK_LENGTH,
            used_by_segment: false,
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ────────────────────────────────────────────────────

    #[test]
    fn test_new_default_block_length() {
        let piece = Piece::new(5, 65536);
        assert_eq!(piece.index(), 5);
        assert_eq!(piece.length(), 65536);
        assert_eq!(piece.block_length(), DEFAULT_BLOCK_LENGTH);
        assert_eq!(piece.count_blocks(), 4); // 65536 / 16384 = 4
        assert_eq!(piece.count_completed_blocks(), 0);
        assert_eq!(piece.count_missing_blocks(), 4);
        assert!(!piece.is_complete());
    }

    #[test]
    fn test_new_custom_block_length() {
        let piece = Piece::with_block_length(0, 32768, 8192);
        assert_eq!(piece.count_blocks(), 4); // 32768 / 8192 = 4
        assert_eq!(piece.block_length(), 8192);
    }

    #[test]
    fn test_new_non_aligned_length() {
        // 50000 bytes with 16384 block length = ceil(50000/16384) = 4 blocks
        // Last block length = 50000 - 3*16384 = 50000 - 49152 = 848
        let piece = Piece::new(0, 50000);
        assert_eq!(piece.count_blocks(), 4);
        assert_eq!(piece.block_length_at(0), 16384);
        assert_eq!(piece.block_length_at(1), 16384);
        assert_eq!(piece.block_length_at(2), 16384);
        assert_eq!(piece.block_length_at(3), 848); // last block
        assert_eq!(piece.block_length_at(4), 0); // out of range
    }

    #[test]
    fn test_new_zero_length() {
        let piece = Piece::new(0, 0);
        assert_eq!(piece.count_blocks(), 0);
        assert!(piece.is_complete()); // vacuously true
        assert_eq!(piece.completed_length(), 0);
    }

    #[test]
    fn test_default() {
        let piece = Piece::default();
        assert_eq!(piece.index(), 0);
        assert_eq!(piece.length(), 0);
        assert_eq!(piece.count_blocks(), 0);
    }

    // ── Block completion ────────────────────────────────────────────────

    #[test]
    fn test_complete_block() {
        let mut piece = Piece::new(0, 65536);
        assert_eq!(piece.count_completed_blocks(), 0);

        piece.complete_block(0);
        assert!(piece.has_block(0));
        assert_eq!(piece.count_completed_blocks(), 1);

        piece.complete_block(1);
        assert!(piece.has_block(1));
        assert_eq!(piece.count_completed_blocks(), 2);
    }

    #[test]
    fn test_complete_all_blocks() {
        let mut piece = Piece::new(0, 65536);
        for i in 0..4 {
            piece.complete_block(i);
        }
        assert!(piece.is_complete());
        assert_eq!(piece.completed_length(), 65536);
    }

    #[test]
    fn test_clear_all_blocks() {
        let mut piece = Piece::new(0, 65536);
        for i in 0..4 {
            piece.complete_block(i);
        }
        assert!(piece.is_complete());

        piece.clear_all_blocks();
        assert!(!piece.is_complete());
        assert_eq!(piece.count_completed_blocks(), 0);
    }

    // ── Missing unused block ────────────────────────────────────────────

    #[test]
    fn test_get_missing_unused_block_index() {
        let mut piece = Piece::new(0, 65536);

        // All blocks are missing and unused
        assert_eq!(piece.get_missing_unused_block_index(), Some(0));

        // Mark block 0 as in-use
        piece.set_block_in_use(0);
        assert_eq!(piece.get_missing_unused_block_index(), Some(1));

        // Mark block 1 as completed
        piece.complete_block(1);
        assert_eq!(piece.get_missing_unused_block_index(), Some(2));
    }

    // ── In-use tracking ─────────────────────────────────────────────────

    #[test]
    fn test_set_and_clear_block_in_use() {
        let mut piece = Piece::new(0, 65536);

        piece.set_block_in_use(2);
        assert!(piece.is_block_in_use(2));
        assert!(!piece.is_block_in_use(0));

        piece.clear_block_in_use(2);
        assert!(!piece.is_block_in_use(2));
    }

    // ── Completed length ────────────────────────────────────────────────

    #[test]
    fn test_completed_length_partial() {
        let mut piece = Piece::new(0, 65536);
        piece.complete_block(0);
        assert_eq!(piece.completed_length(), 16384);

        piece.complete_block(1);
        assert_eq!(piece.completed_length(), 32768);
    }

    #[test]
    fn test_completed_length_non_aligned() {
        let mut piece = Piece::new(0, 50000);
        // Complete all but last block
        piece.complete_block(0);
        piece.complete_block(1);
        piece.complete_block(2);
        assert_eq!(piece.completed_length(), 3 * 16384); // 49152

        piece.complete_block(3); // Last block = 848 bytes
        assert_eq!(piece.completed_length(), 50000);
    }

    // ── User tracking ───────────────────────────────────────────────────

    #[test]
    fn test_add_remove_user() {
        let mut piece = Piece::new(0, 65536);

        piece.add_user(42);
        assert_eq!(piece.user_count(), 1);

        piece.add_user(100);
        assert_eq!(piece.user_count(), 2);

        piece.remove_user(42);
        assert_eq!(piece.user_count(), 1);

        piece.remove_user(100);
        assert_eq!(piece.user_count(), 0);
    }

    // ── Used by segment ─────────────────────────────────────────────────

    #[test]
    fn test_used_by_segment() {
        let mut piece = Piece::new(0, 65536);
        assert!(!piece.is_used_by_segment());

        piece.set_used_by_segment(true);
        assert!(piece.is_used_by_segment());

        piece.set_used_by_segment(false);
        assert!(!piece.is_used_by_segment());
    }

    // ── Hash verification ───────────────────────────────────────────────

    #[test]
    fn test_hash_update_and_digest() {
        let mut piece = Piece::new(0, 4);
        piece.set_hash_type("sha-1");

        assert!(piece.update_hash(0, b"test"));
        assert!(piece.is_hash_calculated());

        let digest = piece.get_digest();
        assert!(digest.is_some());
        // SHA1 of "test" = 0xa94a8fe5ccb19ba61c4c0873d391e987982fbbd3
        assert_eq!(digest.unwrap().len(), 20);
    }

    #[test]
    fn test_hash_offset_mismatch() {
        let mut piece = Piece::new(0, 8);
        piece.set_hash_type("sha-1");

        assert!(piece.update_hash(0, b"tes"));
        assert!(!piece.update_hash(5, b"t")); // offset mismatch
        assert!(piece.update_hash(3, b"t")); // correct offset
    }

    #[test]
    fn test_hash_no_type() {
        let mut piece = Piece::new(0, 4);
        assert!(!piece.update_hash(0, b"test"));
    }

    #[test]
    fn test_destroy_hash_context() {
        let mut piece = Piece::new(0, 4);
        piece.set_hash_type("sha-1");
        piece.update_hash(0, b"test");
        assert!(piece.is_hash_calculated());

        piece.destroy_hash_context();
        assert!(!piece.is_hash_calculated());
    }

    // ── Ordering and equality ───────────────────────────────────────────

    #[test]
    fn test_piece_ordering() {
        let p1 = Piece::new(1, 65536);
        let p2 = Piece::new(2, 65536);
        let p3 = Piece::new(1, 32768);

        assert!(p1 < p2);
        assert!(p1 == p3); // Same index regardless of length
    }

    #[test]
    fn test_piece_debug_format() {
        let piece = Piece::new(5, 65536);
        let debug_str = format!("{:?}", piece);
        assert!(debug_str.contains("index: 5"));
        assert!(debug_str.contains("length: 65536"));
    }

    #[test]
    fn test_piece_display_format() {
        let piece = Piece::new(5, 65536);
        let display_str = format!("{}", piece);
        assert!(display_str.contains("index=5"));
        assert!(display_str.contains("length=65536"));
    }
}
