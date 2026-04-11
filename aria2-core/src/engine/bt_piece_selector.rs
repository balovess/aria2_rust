//! BT Piece Selector - Piece selection strategies
//!
//! This module implements various piece selection algorithms to optimize
//! download performance in BitTorrent clients.
//!
//! # Strategies
//!
//! - **Sequential**: Download pieces in order (simple, predictable)
//! - **Rarest First**: Prioritize pieces that fewest peers have (improves swarm health)
//! - **Random**: Random selection for diversity
//! - **Endgame Mode**: Aggressively request all remaining pieces when few are left
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PieceSelector.h` - Piece selection interface
//! - `src/RarestPieceSelector.cc/h` - Rarest first implementation
//! - `src/PriorityPieceSelector.cc/h` - Priority-based selection
//! - `src/StreamPieceSelector.cc/h` - Sequential streaming

use tracing::{debug, info, warn};

/// Endgame mode threshold: enable when this many pieces remain
pub const ENDGAME_THRESHOLD: u32 = 20;

/// Piece selector configuration
#[derive(Debug, Clone)]
pub struct PieceSelectorConfig {
    /// Enable endgame mode when remaining pieces <= threshold
    pub endgame_threshold: u32,
    /// Prefer rarest pieces first (improves swarm health)
    pub prefer_rarest: bool,
    /// Use strict priority for sequential/priority mode
    pub strict_priority: bool,
}

impl Default for PieceSelectorConfig {
    fn default() -> Self {
        Self {
            endgame_threshold: ENDGAME_THRESHOLD,
            prefer_rarest: true,
            strict_priority: false,
        }
    }
}

/// Result of piece selection operation
pub struct PieceSelectionResult {
    /// Index of the selected piece (if any)
    pub piece_index: Option<usize>,
    /// Whether we're in endgame mode
    pub is_endgame: bool,
    /// Number of pieces remaining
    pub remaining_count: usize,
}

/// BT Piece Selector - Manages which piece to download next
///
/// Wraps the underlying PiecePicker from aria2-protocol and adds
/// higher-level strategy logic including endgame mode detection.
pub struct BtPieceSelector {
    config: PieceSelectorConfig,
    num_pieces: u32,
}

impl BtPieceSelector {
    /// Create a new piece selector with default configuration
    pub fn new(num_pieces: u32) -> Self {
        Self {
            config: PieceSelectorConfig::default(),
            num_pieces,
        }
    }

    /// Create a piece selector with custom configuration
    pub fn with_config(num_pieces: u32, config: PieceSelectorConfig) -> Self {
        Self { config, num_pieces }
    }

    /// Select the next piece to download
    ///
    /// Implements the main selection strategy:
    /// 1. Check if we should enter endgame mode
    /// 2. Apply the configured strategy (rarest first / sequential / random)
    /// 3. Return the selected piece index or None if no pieces available
    ///
    /// # Arguments
    /// * `piece_picker` - The mutable piece picker from aria2-protocol
    /// * `remaining` - Number of incomplete pieces
    ///
    /// # Returns
    /// * `PieceSelectionResult` containing the selected piece and state info
    pub fn select_next_piece(
        &self,
        piece_picker: &mut aria2_protocol::bittorrent::piece::picker::PiecePicker,
        remaining: usize,
    ) -> PieceSelectionResult {
        let is_endgame = remaining > 0 && remaining <= self.config.endgame_threshold as usize;

        if is_endgame && !piece_picker.endgame_candidates().is_empty() {
            warn!("[BT] === ENDGAME MODE === ({} pieces remaining)", remaining);
        }

        let next_piece_idx = if is_endgame {
            // In endgame mode, pick from endgame candidates
            piece_picker.pick_next()
        } else {
            // Normal mode: use configured strategy
            let all_ones_bf = vec![0xFFu8; (self.num_pieces as usize).div_ceil(8)];
            piece_picker.select(&all_ones_bf, self.num_pieces as usize)
        }
        .map(|v| v as usize);

        debug!(
            "[BT] Selected piece: {:?} (endgame={}, remaining={})",
            next_piece_idx, is_endgame, remaining
        );

        PieceSelectionResult {
            piece_index: next_piece_idx,
            is_endgame,
            remaining_count: remaining,
        }
    }

    /// Calculate the actual length of a specific piece
    ///
    /// The last piece may be shorter than the standard piece length.
    ///
    /// # Arguments
    /// * `piece_index` - Index of the piece
    /// * `piece_length` - Standard piece length
    /// * `total_size` - Total torrent size in bytes
    ///
    /// # Returns
    /// * Actual byte length of the specified piece
    pub fn calculate_piece_length(
        &self,
        piece_index: usize,
        piece_length: u32,
        total_size: u64,
    ) -> u32 {
        if piece_index == self.num_pieces as usize - 1
            && !total_size.is_multiple_of(piece_length as u64)
        {
            (total_size % piece_length as u64) as u32
        } else {
            piece_length
        }
    }

    /// Calculate number of blocks in a piece
    ///
    /// # Arguments
    /// * `piece_length` - Length of the piece in bytes
    /// * `block_size` - Size of each block (typically 16KB)
    ///
    /// # Returns
    /// * Number of blocks needed to transfer this piece
    pub fn calculate_num_blocks(piece_length: u32, block_size: u32) -> u32 {
        piece_length.div_ceil(block_size)
    }

    /// Initialize peer frequency tracking for rarest-first strategy
    ///
    /// Updates the piece picker with frequency data from peers' bitfields
    /// to enable rarest-first selection.
    ///
    /// # Arguments
    /// * `piece_picker` - Mutable reference to the piece picker
    /// * `peer_tracker` - Peer bitfield tracker with frequency data
    pub fn initialize_frequencies(
        &self,
        piece_picker: &mut aria2_protocol::bittorrent::piece::picker::PiecePicker,
        peer_tracker: &aria2_protocol::bittorrent::piece::peer_tracker::PeerBitfieldTracker,
    ) {
        piece_picker.set_frequencies_from_peers(&peer_tracker.piece_frequencies());

        info!(
            "[BT] Piece selection initialized: {} pieces, {} peers tracked",
            self.num_pieces,
            peer_tracker.peer_count()
        );
    }

    /// Check if download is complete
    ///
    /// # Arguments
    /// * `piece_picker` - Reference to the piece picker
    ///
    /// # Returns
    /// * `true` if all pieces are marked complete
    pub fn is_complete(
        piece_picker: &aria2_protocol::bittorrent::piece::picker::PiecePicker,
    ) -> bool {
        piece_picker.is_complete()
    }

    /// Get total number of pieces
    pub fn num_pieces(&self) -> u32 {
        self.num_pieces
    }
}

/// Helper functions for piece management

/// Build a bitfield vector from completed pieces
///
/// Creates a bitfield representation where each bit indicates whether
/// the corresponding piece has been completed.
///
/// # Arguments
/// * `num_pieces` - Total number of pieces
/// * `is_completed` - Function that returns true if a piece index is complete
///
/// # Returns
/// * Vector of bytes representing the bitfield (MSB first)
pub fn build_bitfield_from_completed<F>(num_pieces: u32, is_completed: F) -> Vec<u8>
where
    F: Fn(u32) -> bool,
{
    let bf_len = (num_pieces as usize).div_ceil(8);
    let mut bitfield = vec![0u8; bf_len];

    for i in 0..num_pieces {
        if is_completed(i) {
            let byte_idx = (i as usize) / 8;
            let bit_idx = 7 - ((i as usize) % 8); // MSB first
            if byte_idx < bitfield.len() {
                bitfield[byte_idx] |= 1 << bit_idx;
            }
        }
    }

    bitfield
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_endgame_threshold_constant() {
        assert_eq!(ENDGAME_THRESHOLD, 20);
    }

    #[test]
    fn test_piece_selector_config_default() {
        let config = PieceSelectorConfig::default();
        assert_eq!(config.endgame_threshold, 20);
        assert!(config.prefer_rarest);
        assert!(!config.strict_priority);
    }

    #[test]
    fn test_calculate_piece_length_normal() {
        let selector = BtPieceSelector::new(100);
        let length = selector.calculate_piece_length(50, 256 * 1024, 100 * 256 * 1024);
        assert_eq!(length, 256 * 1024); // Normal piece
    }

    #[test]
    fn test_calculate_piece_length_last_piece_shorter() {
        let selector = BtPieceSelector::new(10);
        // Total size = 10 * 1024 - 100 = 9900 (last piece is shorter)
        let total_size = 10 * 1024u64 - 100;
        let length = selector.calculate_piece_length(9, 1024, total_size);
        assert_eq!(length, 924); // Last piece is shorter
    }

    #[test]
    fn test_calculate_num_blocks_exact() {
        let blocks = BtPieceSelector::calculate_num_blocks(16384, 16384);
        assert_eq!(blocks, 1); // Exactly one block
    }

    #[test]
    fn test_calculate_num_blocks_partial() {
        let blocks = BtPieceSelector::calculate_num_blocks(20000, 16384);
        assert_eq!(blocks, 2); // Two blocks (16384 + 3616)
    }

    #[test]
    fn test_build_bitfield_all_complete() {
        let bf = build_bitfield_from_completed(16, |_| true);
        assert_eq!(bf.len(), 2); // 16 bits = 2 bytes
        assert_eq!(bf[0], 0xFF); // All ones
        assert_eq!(bf[1], 0xFF);
    }

    #[test]
    fn test_build_bitfield_none_complete() {
        let bf = build_bitfield_from_completed(8, |_| false);
        assert_eq!(bf.len(), 1); // 8 bits = 1 byte
        assert_eq!(bf[0], 0x00); // All zeros
    }

    #[test]
    fn test_build_bitfield_mixed() {
        let bf = build_bitfield_from_completed(8, |i| i % 2 == 0);
        assert_eq!(bf.len(), 1);
        assert_eq!(bf[0], 0xAA); // 10101010
    }

    #[test]
    fn test_piece_selection_result_default() {
        let result = PieceSelectionResult {
            piece_index: None,
            is_endgame: false,
            remaining_count: 100,
        };
        assert!(result.piece_index.is_none());
        assert!(!result.is_endgame);
        assert_eq!(result.remaining_count, 100);
    }
}
