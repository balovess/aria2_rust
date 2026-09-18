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
