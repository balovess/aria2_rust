//! Shared types for the piece_storage module.
//!
//! Contains the stream piece selector enum, the HaveEntry struct,
//! and the END_GAME_PIECE_NUM constant used across sub-modules.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default number of remaining pieces that trigger end-game mode.
pub(crate) const END_GAME_PIECE_NUM: usize = 20;

// ---------------------------------------------------------------------------
// StreamPieceSelectorKind — HTTP/FTP piece selection strategies
// ---------------------------------------------------------------------------

/// Enum dispatch for stream (HTTP/FTP) piece selection strategies.
///
/// Replaces C++ `StreamPieceSelector` hierarchy:
/// - `DefaultStreamPieceSelector` → sparse mid-point selection
/// - `InorderStreamPieceSelector` → sequential from start
/// - `RandomStreamPieceSelector` → random starting point
/// - `GeomStreamPieceSelector` → geometric distribution
///
/// The default is `Default` (sparse), matching C++ behavior when
/// `PREF_STREAM_PIECE_SELECTOR` is empty or "default".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPieceSelectorKind {
    /// Default/sparse: selects the midpoint of the longest missing run.
    /// C++ `DefaultStreamPieceSelector`.
    Default,
    /// Sequential: selects the first missing piece from the beginning.
    /// C++ `InorderStreamPieceSelector`.
    Inorder,
    /// Random: starts at a random offset, then falls back to inorder.
    /// C++ `RandomStreamPieceSelector`.
    Random,
    /// Geometric: uses geometric progression from the last completed piece.
    /// C++ `GeomStreamPieceSelector` with base 1.5.
    Geom,
}

// ---------------------------------------------------------------------------
// HaveEntry — tracks "have" advertisements
// ---------------------------------------------------------------------------

/// Entry tracking a "have" advertisement for a piece.
///
/// Mirrors the C++ `HaveEntry` struct. When a command completes a piece,
/// it advertises it. Other commands query the advertised list to send
/// Have messages to their peers.
pub(crate) struct HaveEntry {
    /// Monotonically increasing sequence number for ordering.
    pub have_index: u64,
    /// The CUID that completed the piece.
    pub cuid: u64,
    /// The piece index that was completed.
    pub index: usize,
    /// Time when this entry was registered (millis since epoch).
    pub registered_time_ms: u64,
}
