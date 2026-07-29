use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Tracks per-block download state for a single piece
///
/// This state machine provides fine-grained tracking of block-level download progress,
/// enabling detection of stalled downloads and intelligent re-request from different peers.
///
/// # Lifecycle
///
/// 1. **Created** when a piece is selected for download
/// 2. **Blocks requested** via `mark_block_requested()`
/// 3. **Blocks received** via `mark_block_received()` (updates last_activity)
/// 4. **Stalled detection** via `is_stalled(timeout)` - if no activity for N seconds
/// 5. **Complete** when all blocks received (`is_complete() == true`)
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::engine::bt_piece_downloader::PieceDownloadState;
///
/// let mut state = PieceDownloadState::new(0, 262144, 16384); // piece 0, 256KB, 16KB blocks
/// assert_eq!(state.total_blocks, 16); // 256KB / 16KB = 16 blocks
/// assert!(!state.is_complete());
///
/// state.mark_block_requested(0);
/// state.mark_block_received(0);
/// assert_eq!(state.blocks_remaining(), 15);
/// ```
#[derive(Debug, Clone)]
pub struct PieceDownloadState {
    /// Index of this piece in the torrent
    pub piece_index: u32,
    /// Total number of blocks in this piece
    pub total_blocks: u32,
    /// Set of block indices that have been fully received
    pub completed_blocks: HashSet<u32>,
    /// Map of block_index → request_time for pending requests
    pub requested_blocks: HashMap<u32, Instant>,
    /// Timestamp of last successful block receive or cancellation
    pub last_activity: Instant,
}

impl PieceDownloadState {
    /// Create a new PieceDownloadState for the given piece parameters
    ///
    /// # Arguments
    /// * `piece_index` - Index of the piece being tracked
    /// * `piece_length` - Total length of this piece in bytes
    /// * `block_size` - Size of each block (typically 16KB)
    ///
    /// # Returns
    /// * Initialized state with no completed or requested blocks
    pub fn new(piece_index: u32, piece_length: u32, block_size: u32) -> Self {
        let total_blocks = if block_size > 0 {
            piece_length.div_ceil(block_size)
        } else {
            0
        };

        Self {
            piece_index,
            total_blocks,
            completed_blocks: HashSet::new(),
            requested_blocks: HashMap::new(),
            last_activity: Instant::now(),
        }
    }

    /// Get number of blocks still needed to complete this piece
    ///
    /// # Returns
    /// * Count of incomplete blocks (total - completed)
    pub fn blocks_remaining(&self) -> usize {
        (self.total_blocks as usize).saturating_sub(self.completed_blocks.len())
    }

    /// Check if all blocks have been received
    ///
    /// # Returns
    /// * `true` if completed_blocks count >= total_blocks
    pub fn is_complete(&self) -> bool {
        self.completed_blocks.len() as u32 >= self.total_blocks
    }

    /// Check if the download appears stalled (no recent activity with pending requests)
    ///
    /// A piece is considered stalled if:
    /// - It's not yet complete
    /// - There are outstanding requested blocks
    /// - No activity has occurred for longer than timeout_secs
    ///
    /// # Arguments
    /// * `timeout_secs` - Number of seconds without activity to consider stalled
    ///
    /// # Returns
    /// * `true` if the piece appears stuck and may need re-requesting
    pub fn is_stalled(&self, timeout_secs: u64) -> bool {
        !self.is_complete()
            && self.last_activity.elapsed().as_secs() > timeout_secs
            && !self.requested_blocks.is_empty()
    }

    /// Mark a block as requested (sent Request message to peer)
    ///
    /// Updates both the requested_blocks map and last_activity timestamp.
    ///
    /// # Arguments
    /// * `block_index` - Index of the block within this piece
    pub fn mark_block_requested(&mut self, block_index: u32) {
        self.requested_blocks.insert(block_index, Instant::now());
        self.last_activity = Instant::now();
    }

    /// Mark a block as received (got Piece message from peer)
    ///
    /// Moves the block from requested to completed and updates last_activity.
    ///
    /// # Arguments
    /// * `block_index` - Index of the block within this piece
    pub fn mark_block_received(&mut self, block_index: u32) {
        self.completed_blocks.insert(block_index);
        self.requested_blocks.remove(&block_index);
        self.last_activity = Instant::now();
    }

    /// Mark a block as cancelled (sent Cancel message or gave up)
    ///
    /// Removes the block from requested but does NOT add to completed.
    /// Does not update last_activity (cancellation isn't progress).
    ///
    /// # Arguments
    /// * `block_index` - Index of the block within this piece
    pub fn mark_block_cancelled(&mut self, block_index: u32) {
        self.requested_blocks.remove(&block_index);
    }

    /// Get completion percentage (0.0 to 100.0)
    ///
    /// # Returns
    /// * Percentage of blocks that have been received
    pub fn progress_percent(&self) -> f64 {
        if self.total_blocks == 0 {
            return 100.0;
        }
        (self.completed_blocks.len() as f64 / self.total_blocks as f64) * 100.0
    }
}
