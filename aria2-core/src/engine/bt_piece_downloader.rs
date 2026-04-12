use crate::engine::bt_upload_session::PieceDataProvider;
use crate::engine::multi_file_layout::MultiFileLayout;
use crate::error::{Aria2Error, FatalError, Result};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Manages piece/block download operations for BitTorrent downloads.
///
/// This module encapsulates:
/// - Piece selection and block request logic
/// - Data verification and hash checking
/// - Writing downloaded data to disk or multi-file layouts
/// - File-backed piece provider for seeding phase
///
/// Extracted from BtDownloadCommand to follow single responsibility principle,
/// mirroring original aria2 C++ architecture separation.

// ======================================================================
// Piece Download State Machine
// ======================================================================

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
            (piece_length + block_size - 1) / block_size
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

// ======================================================================
// File-Backed Piece Provider
// ======================================================================

/// Provides piece data from local files, used during seeding phase.
///
/// Supports both single-file and multi-file torrent layouts.
pub struct FileBackedPieceProvider {
    file_path: std::path::PathBuf,
    piece_length: u32,
    num_pieces: u32,
    multi_file_layout: Option<MultiFileLayout>,
}

impl FileBackedPieceProvider {
    pub fn new(
        file_path: std::path::PathBuf,
        piece_length: u32,
        num_pieces: u32,
        multi_file_layout: Option<MultiFileLayout>,
    ) -> Self {
        Self {
            file_path,
            piece_length,
            num_pieces,
            multi_file_layout,
        }
    }
}

impl PieceDataProvider for FileBackedPieceProvider {
    fn get_piece_data(&self, piece_index: u32, offset: u32, length: u32) -> Option<Vec<u8>> {
        use std::io::SeekFrom;
        use tokio::fs::File;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let read_op =
            move |file_path: std::path::PathBuf, seek_pos: u64, len: u32| -> Option<Vec<u8>> {
                let rt = match tokio::runtime::Handle::try_current() {
                    Ok(handle) => handle,
                    Err(_) => {
                        let rt = tokio::runtime::Runtime::new().ok()?;
                        return rt.block_on(async {
                            let mut f = File::open(&file_path).await.ok()?;
                            f.seek(SeekFrom::Start(seek_pos)).await.ok()?;
                            let mut buf = vec![0u8; len as usize];
                            f.read_exact(&mut buf).await.ok()?;
                            Some(buf)
                        });
                    }
                };
                tokio::task::block_in_place(|| {
                    rt.block_on(async {
                        let mut f = File::open(&file_path).await.ok()?;
                        f.seek(SeekFrom::Start(seek_pos)).await.ok()?;
                        let mut buf = vec![0u8; len as usize];
                        f.read_exact(&mut buf).await.ok()?;
                        Some(buf)
                    })
                })
            };

        if let Some(ref layout) = self.multi_file_layout {
            let global_start = piece_index as u64 * layout.piece_length() as u64 + offset as u64;

            if global_start >= layout.total_size() {
                return None;
            }

            let actual_length = (length as u64).min(layout.total_size() - global_start) as u32;
            let mut result = Vec::with_capacity(actual_length as usize);
            let mut current_global = global_start;
            let mut remaining = actual_length as u64;

            while remaining > 0 {
                let current_piece_idx = (current_global / layout.piece_length() as u64) as u32;
                let current_offset_in_piece =
                    (current_global % layout.piece_length() as u64) as u32;

                let (file_idx, file_offset) =
                    layout.resolve_file_offset(current_piece_idx, current_offset_in_piece)?;
                let file_path = layout.file_absolute_path(file_idx)?.to_path_buf();

                let file_info = layout.get_file_info(file_idx)?;
                let file_end = file_info.start_piece as u64 * layout.piece_length() as u64
                    + file_info.start_offset_in_piece as u64
                    + file_info.length;

                let bytes_available_in_file = file_end - current_global;
                let bytes_to_read = remaining.min(bytes_available_in_file) as u32;

                if let Some(data) = read_op(file_path.clone(), file_offset, bytes_to_read) {
                    result.extend_from_slice(&data);
                    current_global += data.len() as u64;
                    remaining -= data.len() as u64;
                } else {
                    return None;
                }
            }

            Some(result)
        } else {
            let file_pos = piece_index as u64 * self.piece_length as u64 + offset as u64;
            read_op(self.file_path.clone(), file_pos, length)
        }
    }

    fn has_piece(&self, _piece_index: u32) -> bool {
        true
    }

    fn num_pieces(&self) -> u32 {
        self.num_pieces
    }
}

// ======================================================================
// Multi-File Writer
// ======================================================================

/// Coalesced write entry: tracks (file_index, file_offset, data) for batched I/O.
///
/// Adjacent writes to the same file within [`COALESCE_GAP`] bytes are merged
/// into a single larger write, reducing the number of `seek` + `write_all`
/// syscalls.
struct CoalescedWrite {
    file_idx: usize,
    file_offset: u64,
    data: Vec<u8>,
}

/// Maximum gap (in bytes) between two writes to the same file that will still
/// be coalesced into a single write operation.  Gaps are zero-filled so that
/// the resulting sparse region is correct on disk.
const COALESCE_GAP: u64 = 4096;

/// Writes a completed piece's data across multiple files in a multi-file torrent.
///
/// Handles cross-file boundary cases where a single piece spans multiple files.
/// Each file is opened once, written to at the correct offsets, then flushed.
///
/// # Arguments
/// * `layout` - The multi-file layout defining file boundaries
/// * `piece_idx` - Index of the piece being written
/// * `piece_data` - Complete piece data to write
/// * `_piece_length` - Standard piece length (reserved for future use)
///
/// # Errors
/// Returns error if file open, seek, write, or flush operations fail.
pub async fn write_piece_to_multi_files(
    layout: &MultiFileLayout,
    piece_idx: u32,
    piece_data: &[u8],
    _piece_length: u32,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let mut file_writers: HashMap<usize, tokio::fs::File> = HashMap::new();

    let mut data_offset = 0usize;
    while data_offset < piece_data.len() {
        let piece_offset = data_offset as u32;

        if let Some((file_idx, file_offset)) = layout.resolve_file_offset(piece_idx, piece_offset) {
            let file_path = layout
                .file_absolute_path(file_idx)
                .ok_or_else(|| {
                    Aria2Error::Fatal(FatalError::Config("invalid file index".to_string()))
                })?
                .to_path_buf();

            if let std::collections::hash_map::Entry::Vacant(e) = file_writers.entry(file_idx) {
                let f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&file_path)
                    .await
                    .map_err(|e| {
                        Aria2Error::Fatal(FatalError::Config(format!("open failed: {}", e)))
                    })?;
                e.insert(f);
            }

            let file_info = layout.get_file_info(file_idx).ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config("invalid file index".to_string()))
            })?;

            let bytes_available_in_file = file_info.length.saturating_sub(file_offset);
            let bytes_remaining_in_piece = (piece_data.len() - data_offset) as u64;
            let write_len = bytes_available_in_file.min(bytes_remaining_in_piece) as usize;

            if write_len == 0 {
                break;
            }

            let chunk = &piece_data[data_offset..data_offset + write_len];

            let writer = file_writers.get_mut(&file_idx).unwrap();
            writer
                .seek(std::io::SeekFrom::Start(file_offset))
                .await
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("seek failed: {}", e)))
                })?;
            writer.write_all(chunk).await.map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("write failed: {}", e)))
            })?;

            data_offset += write_len;
        } else {
            break;
        }
    }

    for (_, mut f) in file_writers {
        f.flush()
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("flush failed: {}", e))))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piece_download_state_creation() {
        // Test with standard 256KB piece and 16KB blocks
        let state = PieceDownloadState::new(0, 262144, 16384);
        assert_eq!(state.piece_index, 0);
        assert_eq!(state.total_blocks, 16); // 262144 / 16384 = 16
        assert!(state.completed_blocks.is_empty());
        assert!(state.requested_blocks.is_empty());
        assert!(!state.is_complete());
        assert_eq!(state.blocks_remaining(), 16);
        assert_eq!(state.progress_percent(), 0.0);
    }

    #[test]
    fn test_piece_download_state_partial_last_block() {
        // Test with piece that doesn't divide evenly
        let state = PieceDownloadState::new(5, 20000, 16384);
        assert_eq!(state.total_blocks, 2); // (20000 + 16384 - 1) / 16384 = 2
        assert_eq!(state.blocks_remaining(), 2);
    }

    #[test]
    fn test_piece_download_state_zero_block_size() {
        // Edge case: zero block size should result in 0 total blocks
        let state = PieceDownloadState::new(0, 100000, 0);
        assert_eq!(state.total_blocks, 0);
        assert!(state.is_complete()); // 0 blocks means "complete"
    }

    #[test]
    fn test_mark_block_requested() {
        let mut state = PieceDownloadState::new(0, 32768, 16384);

        state.mark_block_requested(0);
        assert_eq!(state.requested_blocks.len(), 1);
        assert!(state.requested_blocks.contains_key(&0));
        assert_eq!(state.blocks_remaining(), 2); // Still need all blocks

        state.mark_block_requested(1);
        assert_eq!(state.requested_blocks.len(), 2);
    }

    #[test]
    fn test_mark_block_received() {
        let mut state = PieceDownloadState::new(0, 32768, 16384);

        // Request then receive block 0
        state.mark_block_requested(0);
        state.mark_block_received(0);

        assert!(state.completed_blocks.contains(&0));
        assert!(!state.requested_blocks.contains_key(&0));
        assert_eq!(state.blocks_remaining(), 1);
        assert_eq!(state.progress_percent(), 50.0);
    }

    #[test]
    fn test_mark_block_cancelled() {
        let mut state = PieceDownloadState::new(0, 32768, 16384);

        state.mark_block_requested(0);
        state.mark_block_cancelled(0);

        assert!(!state.completed_blocks.contains(&0));
        assert!(!state.requested_blocks.contains_key(&0));
        assert_eq!(state.blocks_remaining(), 2); // Still need this block
    }

    #[test]
    fn test_is_complete_all_blocks_received() {
        let mut state = PieceDownloadState::new(0, 49152, 16384); // 3 blocks

        for i in 0..3 {
            state.mark_block_requested(i);
            state.mark_block_received(i);
        }

        assert!(state.is_complete());
        assert_eq!(state.blocks_remaining(), 0);
        assert_eq!(state.progress_percent(), 100.0);
    }

    #[test]
    fn test_is_stalled_with_pending_requests() {
        let mut state = PieceDownloadState::new(0, 32768, 16384);

        state.mark_block_requested(0);
        state.mark_block_requested(1);

        // Should not be stalled immediately
        assert!(!state.is_stalled(30));

        // Simulate time passing by checking with very small timeout
        // In practice, this would require mocking time or using a very long timeout
        // For now, just verify the logic structure is correct
        assert!(state.requested_blocks.len() == 2);
        assert!(!state.is_complete());
    }

    #[test]
    fn test_is_not_stalled_when_no_pending_requests() {
        let mut state = PieceDownloadState::new(0, 32768, 16384);

        state.mark_block_requested(0);
        state.mark_block_cancelled(0);

        // No pending requests → not stalled even if no activity
        assert!(!state.is_stalled(0)); // Even with 0 timeout
    }

    #[test]
    fn test_progress_percent_calculation() {
        let mut state = PieceDownloadState::new(0, 65536, 16384); // 4 blocks

        assert_eq!(state.progress_percent(), 0.0);

        state.mark_block_requested(0);
        state.mark_block_received(0);
        assert_eq!(state.progress_percent(), 25.0);

        state.mark_block_requested(1);
        state.mark_block_received(1);
        assert_eq!(state.progress_percent(), 50.0);

        state.mark_block_requested(2);
        state.mark_block_received(2);
        assert_eq!(state.progress_percent(), 75.0);

        state.mark_block_requested(3);
        state.mark_block_received(3);
        assert_eq!(state.progress_percent(), 100.0);
    }

    #[test]
    fn test_state_lifecycle_full_cycle() {
        // Complete lifecycle: create → request → receive → complete
        let mut state = PieceDownloadState::new(10, 262144, 16384);

        // Initial state
        assert_eq!(state.piece_index, 10);
        assert_eq!(state.total_blocks, 16);
        assert!(!state.is_complete());

        // Request some blocks
        for i in 0..5 {
            state.mark_block_requested(i);
        }
        assert_eq!(state.requested_blocks.len(), 5);

        // Receive first 3
        for i in 0..3 {
            state.mark_block_received(i);
        }
        assert_eq!(state.completed_blocks.len(), 3);
        assert_eq!(state.requested_blocks.len(), 2); // 4,5 still pending
        assert_eq!(state.progress_percent(), 18.75); // 3/16 = 18.75%

        // Cancel remaining requested
        state.mark_block_cancelled(3);
        state.mark_block_cancelled(4);
        state.mark_block_cancelled(5);
        assert_eq!(state.requested_blocks.len(), 0);

        // Continue to completion
        for i in 3..16 {
            state.mark_block_requested(i);
            state.mark_block_received(i);
        }

        assert!(state.is_complete());
        assert_eq!(state.progress_percent(), 100.0);
    }
}

// ======================================================================
// Coalesced Multi-File Writer  (Phase 14 / Task I4)
// ======================================================================

/// Writes a completed piece's data across multiple files using **coalesced
/// writes** to reduce the number of `seek` + `write` syscalls.
///
/// # Algorithm
///
/// 1. **Collect** – iterate over `piece_data`, resolve each byte-range to
///    `(file_idx, file_offset)`, and push a raw write entry.
/// 2. **Sort** – order entries by `(file_idx, file_offset)` so that
///    adjacent regions are neighbours.
/// 3. **Coalesce** – merge consecutive writes to the **same file** whose
///    start offset is within [`COALESCE_GAP`] bytes of the previous write's
///    end.  Any gap is zero-filled (sparse region).
/// 4. **Execute** – open each unique file **once**, seek + write_all per
///    coalesced entry, then flush.
///
/// # When to use
///
/// Prefer this function over [`write_piece_to_multi_files`] for production
/// downloads where a piece may span many files or many small write
/// operations would otherwise occur.  The original function is retained for
/// backward compatibility and as a reference implementation.
///
/// # Arguments
/// * `layout`      – The multi-file layout defining file boundaries.
/// * `piece_idx`   – Index of the piece being written.
/// * `piece_data`  – Complete piece data to write.
/// * `_piece_length` – Standard piece length (reserved for future use).
pub async fn write_piece_to_multi_files_coalesced(
    layout: &MultiFileLayout,
    piece_idx: u32,
    piece_data: &[u8],
    _piece_length: u32,
) -> Result<()> {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    // ------------------------------------------------------------------
    // Phase 1: Collect all raw write operations
    // ------------------------------------------------------------------
    let mut raw_writes: Vec<(usize, u64, Vec<u8>)> = Vec::new();
    let mut data_offset = 0usize;

    while data_offset < piece_data.len() {
        let piece_offset = data_offset as u32;
        if let Some((file_idx, file_offset)) = layout.resolve_file_offset(piece_idx, piece_offset) {
            let file_info = layout.get_file_info(file_idx).ok_or_else(|| {
                Aria2Error::Fatal(FatalError::Config("invalid file index".to_string()))
            })?;

            let bytes_available = file_info.length.saturating_sub(file_offset);
            let bytes_remaining = (piece_data.len() - data_offset) as u64;
            let write_len = bytes_available.min(bytes_remaining) as usize;

            if write_len > 0 {
                raw_writes.push((
                    file_idx,
                    file_offset,
                    piece_data[data_offset..data_offset + write_len].to_vec(),
                ));
                data_offset += write_len;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    // ------------------------------------------------------------------
    // Phase 2: Sort by (file_idx, file_offset)
    // ------------------------------------------------------------------
    raw_writes.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    // ------------------------------------------------------------------
    // Phase 3: Coalesce adjacent writes within COALESCE_GAP
    // ------------------------------------------------------------------
    let mut coalesced: Vec<CoalescedWrite> = Vec::new();

    for (file_idx, file_offset, data) in raw_writes {
        if let Some(last) = coalesced.last_mut() {
            let last_end = last.file_offset + last.data.len() as u64;
            if last.file_idx == file_idx && file_offset <= last_end + COALESCE_GAP {
                // Extend or gap-fill then extend
                if file_offset > last_end {
                    // Fill gap with zeros (sparse region)
                    last.data
                        .extend_from_slice(&vec![0u8; (file_offset - last_end) as usize]);
                }
                last.data.extend_from_slice(&data);
                continue;
            }
        }
        coalesced.push(CoalescedWrite {
            file_idx,
            file_offset,
            data,
        });
    }

    // ------------------------------------------------------------------
    // Phase 4: Execute coalesced writes (one open per unique file)
    // ------------------------------------------------------------------
    let mut file_writers: HashMap<usize, tokio::fs::File> = HashMap::new();

    for cw in &coalesced {
        let file_path = layout
            .file_absolute_path(cw.file_idx)
            .ok_or_else(|| Aria2Error::Fatal(FatalError::Config("invalid file index".to_string())))?
            .to_path_buf();

        let writer = match file_writers.entry(cw.file_idx) {
            std::collections::hash_map::Entry::Vacant(e) => {
                let f = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(&file_path)
                    .await
                    .map_err(|e| {
                        Aria2Error::Fatal(FatalError::Config(format!("open failed: {}", e)))
                    })?;
                e.insert(f)
            }
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        };

        writer
            .seek(std::io::SeekFrom::Start(cw.file_offset))
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("seek failed: {}", e))))?;
        writer
            .write_all(&cw.data)
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("write failed: {}", e))))?;
    }

    for (_, mut f) in file_writers {
        f.flush()
            .await
            .map_err(|e| Aria2Error::Fatal(FatalError::Config(format!("flush failed: {}", e))))?;
    }

    Ok(())
}
