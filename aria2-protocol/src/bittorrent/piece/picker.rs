//! Piece picker module
//!
//! Implements piece selection strategies for BitTorrent downloads.
//! Based on BEP 0019 (WebSeed) and various piece picking algorithms.

/// Piece selection strategy — determines the algorithm used to pick the next
/// piece to request from peers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceSelectionStrategy {
    /// Select pieces sequentially (good for streaming)
    Sequential,
    /// Select rarest pieces first (default BitTorrent strategy)
    RarestFirst,
    /// Select pieces randomly
    Random,
    /// Select pieces to form the longest contiguous sequence
    LongestSequence,
    /// Priority-based selection (higher priority pieces first)
    Priority,
    /// Geometric distribution (prefer earlier pieces)
    Geometric,
}

/// Piece priority mode — controls how pieces are prioritised within the
/// picker, independent of the base selection strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePriorityMode {
    /// Prioritise pieces from the beginning of the file (head)
    SequentialHead,
    /// Prioritise pieces from the end of the file (tail)
    SequentialTail,
    /// Default rarest-first priority (no special head/tail bias)
    RarestFirst,
}

/// Legacy alias kept for backward compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiecePickStrategy {
    /// Select pieces sequentially (good for streaming)
    Sequential,
    /// Select rarest pieces first (default BitTorrent strategy)
    RarestFirst,
    /// Select pieces randomly
    Random,
    /// Select pieces to form the longest contiguous sequence
    LongestSequence,
    /// Priority-based selection (higher priority pieces first)
    Priority,
    /// Geometric distribution (prefer earlier pieces)
    Geometric,
}

/// Information about a single piece within the picker.
///
/// Returned by [`PiecePicker::get_piece_info`].
#[derive(Debug, Clone)]
pub struct PieceInfo {
    /// Zero-based piece index
    pub index: u32,
    /// Number of peers that have this piece (availability frequency)
    pub frequency: u32,
    /// Whether this piece has been fully downloaded and verified
    pub is_completed: bool,
    /// Whether this piece has been fully downloaded and verified (alias)
    pub completed: bool,
    /// Whether this piece is currently being downloaded
    pub in_progress: bool,
    /// Priority level (0 = default, higher = more important)
    pub priority: u8,
}

/// Piece picker — selects the next piece to download based on the
/// configured strategy, peer bitfield frequency data, and priority mode.
///
/// Maintains internal state for end-game candidate tracking and
/// per-piece frequency counters used by the rarest-first algorithm.
pub struct PiecePicker {
    /// Total number of pieces in the torrent
    num_pieces: u32,
    /// Active selection strategy
    strategy: PieceSelectionStrategy,
    /// Active priority mode
    priority_mode: PiecePriorityMode,
    /// Per-piece availability frequency (from peer bitfields)
    frequencies: Vec<u32>,
    /// Per-piece completion tracking (true = piece verified and written)
    completed: Vec<bool>,
    /// Per-piece in-progress tracking (true = piece is being downloaded)
    in_progress: Vec<bool>,
    /// Per-piece priority (0 = default, higher = more important)
    priorities: Vec<u8>,
    /// Indices of pieces that are candidates for end-game mode
    endgame_candidates: Vec<usize>,
}

impl PiecePicker {
    /// Create a new picker for a torrent with `num_pieces` pieces.
    pub fn new(num_pieces: u32) -> Self {
        let n = num_pieces as usize;
        Self {
            num_pieces,
            strategy: PieceSelectionStrategy::RarestFirst,
            priority_mode: PiecePriorityMode::RarestFirst,
            frequencies: vec![0; n],
            completed: vec![false; n],
            in_progress: vec![false; n],
            priorities: vec![0; n],
            endgame_candidates: Vec::new(),
        }
    }

    /// Set the base selection strategy.
    pub fn set_strategy(&mut self, strategy: PieceSelectionStrategy) {
        self.strategy = strategy;
    }

    /// Set the piece priority mode.
    pub fn set_priority_mode(&mut self, mode: PiecePriorityMode) {
        self.priority_mode = mode;
    }

    /// Pick the next piece index using the configured strategy.
    ///
    /// `bitfield` is the peer's have-bitfield, `nbits` is the
    /// number of valid bits (typically `num_pieces`).
    pub fn select(&self, _bitfield: &[u8], _nbits: usize) -> Option<u32> {
        // TODO: implement actual selection logic per strategy
        None
    }

    /// Pick the next piece in end-game mode (simple sequential scan).
    pub fn pick_next(&self) -> Option<u32> {
        // TODO: implement end-game selection
        None
    }

    /// Return the list of piece indices that are end-game candidates.
    pub fn endgame_candidates(&self) -> &[usize] {
        &self.endgame_candidates
    }

    /// Update per-piece frequency data from a peer frequency slice.
    pub fn set_frequencies_from_peers(&mut self, freqs: &[usize]) {
        let len = freqs.len().min(self.frequencies.len());
        for i in 0..len {
            self.frequencies[i] = freqs[i] as u32;
        }
    }

    /// Iterator over all pieces, yielding [`PieceInfo`] for each.
    pub fn pieces_iter(&self) -> impl Iterator<Item = PieceInfo> + '_ {
        let n = self.num_pieces as usize;
        (0..n).map(move |i| PieceInfo {
            index: i as u32,
            frequency: self.frequencies[i],
            is_completed: self.completed[i],
            completed: self.completed[i],
            in_progress: self.in_progress[i],
            priority: self.priorities[i],
        })
    }

    /// Return info about a specific piece, or `None` if out of range.
    pub fn get_piece_info(&self, index: u32) -> Option<PieceInfo> {
        let i = index as usize;
        if i >= self.num_pieces as usize {
            return None;
        }
        Some(PieceInfo {
            index,
            frequency: self.frequencies[i],
            is_completed: self.completed[i],
            completed: self.completed[i],
            in_progress: self.in_progress[i],
            priority: self.priorities[i],
        })
    }

    /// Return the current priority mode.
    pub fn priority_mode(&self) -> PiecePriorityMode {
        self.priority_mode
    }

    /// Number of pieces not yet completed.
    pub fn remaining_count(&self) -> usize {
        self.completed.iter().filter(|&&c| !c).count()
    }

    /// Mark a piece as completed.
    ///
    /// # Panics
    /// Panics if `index` is out of range in debug builds.
    pub fn mark_completed(&mut self, index: u32) {
        let i = index as usize;
        debug_assert!(
            i < self.num_pieces as usize,
            "mark_completed: index out of range"
        );
        if i < self.num_pieces as usize {
            self.completed[i] = true;
        }
    }

    /// Export completed pieces as a bitfield byte vector (MSB-first).
    pub fn export_bitfield(&self) -> Vec<u8> {
        let n = self.num_pieces as usize;
        let bf_len = n.div_ceil(8);
        let mut bitfield = vec![0u8; bf_len];
        for (i, &done) in self.completed.iter().enumerate() {
            if done {
                let byte_idx = i / 8;
                let bit_idx = 7 - (i % 8);
                bitfield[byte_idx] |= 1 << bit_idx;
            }
        }
        bitfield
    }

    /// Check if all pieces are completed.
    pub fn is_complete(&self) -> bool {
        self.completed.iter().all(|&c| c)
    }
}

/// Piece picker configuration
#[derive(Debug, Clone)]
pub struct PiecePickerConfig {
    /// Selection strategy
    pub strategy: PiecePickStrategy,
    /// Number of pieces to request ahead
    pub request_queue_size: usize,
    /// Whether to prioritize end-game mode
    pub end_game_threshold: f64,
}

impl Default for PiecePickerConfig {
    fn default() -> Self {
        Self {
            strategy: PiecePickStrategy::RarestFirst,
            request_queue_size: 16,
            end_game_threshold: 0.95,
        }
    }
}

/// Result of a piece pick operation
#[derive(Debug, Clone)]
pub struct PickedPiece {
    /// Index of the picked piece
    pub index: usize,
    /// Priority of the piece (higher = more important)
    pub priority: u8,
    /// Whether this piece is in end-game mode
    pub is_end_game: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_piece_selection_strategy_variants() {
        assert_ne!(
            PieceSelectionStrategy::Sequential,
            PieceSelectionStrategy::RarestFirst
        );
        assert_ne!(
            PieceSelectionStrategy::Random,
            PieceSelectionStrategy::Geometric
        );
    }

    #[test]
    fn test_piece_priority_mode_variants() {
        assert_ne!(
            PiecePriorityMode::SequentialHead,
            PiecePriorityMode::SequentialTail
        );
        assert_ne!(
            PiecePriorityMode::SequentialTail,
            PiecePriorityMode::RarestFirst
        );
    }

    #[test]
    fn test_piece_picker_new() {
        let picker = PiecePicker::new(100);
        assert!(picker.endgame_candidates().is_empty());
        assert_eq!(picker.remaining_count(), 100);
        assert!(!picker.is_complete());
    }

    #[test]
    fn test_piece_picker_new_u32() {
        let picker = PiecePicker::new(50u32);
        assert_eq!(picker.remaining_count(), 50);
    }

    #[test]
    fn test_piece_picker_set_strategy() {
        let mut picker = PiecePicker::new(100);
        picker.set_strategy(PieceSelectionStrategy::Sequential);
        // Verify it compiles and runs without panic
        let _ = picker.select(&[0xFF; 13], 100);
    }

    #[test]
    fn test_piece_picker_set_priority_mode() {
        let mut picker = PiecePicker::new(100);
        picker.set_priority_mode(PiecePriorityMode::SequentialHead);
        assert_eq!(picker.priority_mode(), PiecePriorityMode::SequentialHead);
    }

    #[test]
    fn test_piece_pick_strategy_variants() {
        assert_ne!(
            PiecePickStrategy::Sequential,
            PiecePickStrategy::RarestFirst
        );
        assert_ne!(PiecePickStrategy::Random, PiecePickStrategy::Geometric);
    }

    #[test]
    fn test_piece_picker_config_default() {
        let config = PiecePickerConfig::default();
        assert_eq!(config.strategy, PiecePickStrategy::RarestFirst);
        assert_eq!(config.request_queue_size, 16);
        assert!(config.end_game_threshold > 0.9);
    }

    #[test]
    fn test_picked_piece() {
        let picked = PickedPiece {
            index: 42,
            priority: 5,
            is_end_game: false,
        };
        assert_eq!(picked.index, 42);
        assert_eq!(picked.priority, 5);
        assert!(!picked.is_end_game);
    }

    #[test]
    fn test_mark_completed_and_is_complete() {
        let mut picker = PiecePicker::new(3u32);
        assert!(!picker.is_complete());
        assert_eq!(picker.remaining_count(), 3);

        picker.mark_completed(0);
        assert!(!picker.is_complete());
        assert_eq!(picker.remaining_count(), 2);

        picker.mark_completed(1);
        picker.mark_completed(2);
        assert!(picker.is_complete());
        assert_eq!(picker.remaining_count(), 0);
    }

    #[test]
    fn test_export_bitfield() {
        let mut picker = PiecePicker::new(8u32);
        picker.mark_completed(0);
        picker.mark_completed(7);
        let bf = picker.export_bitfield();
        assert_eq!(bf.len(), 1);
        // Bit 0 (MSB) and bit 7 (LSB) set => 10000001 = 0x81
        assert_eq!(bf[0], 0x81);
    }

    #[test]
    fn test_get_piece_info() {
        let mut picker = PiecePicker::new(10u32);
        picker.mark_completed(3);
        picker.set_frequencies_from_peers(&[0, 2, 0, 0, 5, 0, 0, 0, 0, 0]);

        let info = picker.get_piece_info(3).unwrap();
        assert!(info.is_completed);
        assert_eq!(info.index, 3);
        assert_eq!(info.frequency, 0);

        let info4 = picker.get_piece_info(4).unwrap();
        assert!(!info4.is_completed);
        assert_eq!(info4.frequency, 5);

        assert!(picker.get_piece_info(100).is_none());
    }

    #[test]
    fn test_pieces_iter() {
        let picker = PiecePicker::new(5u32);
        let indices: Vec<u32> = picker.pieces_iter().map(|p| p.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_pieces_iter_empty() {
        let picker = PiecePicker::new(0u32);
        let indices: Vec<u32> = picker.pieces_iter().map(|p| p.index).collect();
        assert!(indices.is_empty());
    }

    #[test]
    fn test_export_bitfield_partial() {
        let mut picker = PiecePicker::new(16u32);
        // Set pieces 0, 3, 7
        picker.mark_completed(0);
        picker.mark_completed(3);
        picker.mark_completed(7);
        let bf = picker.export_bitfield();
        // MSB-first: piece0=bit7(0x80), piece3=bit4(0x10), piece7=bit0(0x01)
        // 0x80|0x10|0x01 = 0x91
        assert_eq!(bf.len(), 2);
        assert_eq!(bf[0], 0x91);
        assert_eq!(bf[1], 0x00);
    }

    #[test]
    fn test_is_complete_zero_pieces() {
        let picker = PiecePicker::new(0u32);
        // Zero pieces: vacuously true
        assert!(picker.is_complete());
        assert_eq!(picker.remaining_count(), 0);
    }

    #[test]
    fn test_set_frequencies_from_peers() {
        let mut picker = PiecePicker::new(4u32);
        let freqs: Vec<usize> = vec![3, 1, 5, 2];
        picker.set_frequencies_from_peers(&freqs);

        let info = picker.get_piece_info(2).unwrap();
        assert_eq!(info.frequency, 5);
    }
}
