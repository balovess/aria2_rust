//! Piece picker module
//!
//! Implements piece selection strategies for BitTorrent downloads.
//! Based on BEP 0019 (WebSeed) and various piece picking algorithms.

/// Piece selection strategy
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
