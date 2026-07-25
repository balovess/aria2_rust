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

/// Piece picker — selects the next piece to download based on the
/// configured strategy, peer bitfield frequency data, and priority mode.
///
/// Maintains internal state for end-game candidate tracking and
/// per-piece frequency counters used by the rarest-first algorithm.
pub struct PiecePicker {
    /// Total number of pieces in the torrent
    #[allow(dead_code)] // used by future selection logic
    num_pieces: usize,
    /// Active selection strategy
    strategy: PieceSelectionStrategy,
    /// Active priority mode
    priority_mode: PiecePriorityMode,
    /// Per-piece availability frequency (from peer bitfields)
    frequencies: Vec<u32>,
    /// Indices of pieces that are candidates for end-game mode
    endgame_candidates: Vec<usize>,
}

impl PiecePicker {
    /// Create a new picker for a torrent with `num_pieces` pieces.
    pub fn new(num_pieces: usize) -> Self {
        Self {
            num_pieces,
            strategy: PieceSelectionStrategy::RarestFirst,
            priority_mode: PiecePriorityMode::RarestFirst,
            frequencies: vec![0; num_pieces],
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
    pub fn set_frequencies_from_peers(&mut self, freqs: &[u32]) {
        let len = freqs.len().min(self.frequencies.len());
        self.frequencies[..len].copy_from_slice(&freqs[..len]);
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
    }

    #[test]
    fn test_piece_pick_strategy_variants() {
        assert_ne!(PiecePickStrategy::Sequential, PiecePickStrategy::RarestFirst);
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
}
