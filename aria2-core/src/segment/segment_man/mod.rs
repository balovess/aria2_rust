//! Segment manager coordinating between PieceStorage and download commands.
//!
//! `SegmentMan` is the Rust equivalent of the C++ `SegmentMan` class. It
//! coordinates segment checkout/cancellation/completion with the underlying
//! [`PieceStorage`], tracks in-flight segments per CUID, remembers written
//! lengths for resume support, and manages peer statistics.
//!
//! # Architecture
//!
//! ```text
//!   DownloadCommand ──get_segment()──> SegmentMan ──get_missing_piece()──> PieceStorage
//!                      <──SegmentKind──            <──Piece──
//!
//!   DownloadCommand ──complete_segment()──> SegmentMan ──complete_piece()──> PieceStorage
//!   DownloadCommand ──cancel_segment()────> SegmentMan ──cancel_piece()────> PieceStorage
//! ```
//!
//! # Ownership Model
//!
//! Unlike the C++ version which uses `shared_ptr<Segment>` for shared ownership
//! between `SegmentMan` and the caller, this Rust version uses **move semantics**:
//!
//! - `get_segment()` returns `SegmentKind` (Piece owned by caller)
//! - `used_segment_entries` stores lightweight tracking entries `(cuid, index)`
//! - For `cancel_segment(cuid)`, we interact with `PieceStorage` by index
//! - For `complete_segment` / `cancel_segment_by_segment`, the caller passes
//!   the `SegmentKind` reference back
//!
//! # C++ Reference
//!
//! Based on `SegmentMan.h` / `SegmentMan.cc` from both the original aria2
//! and aria2-next. The aria2-next version adds `cancelSegmentByIndex()` and
//! uses `A2_LOG_TRACE` instead of `A2_LOG_DEBUG`.

pub mod peer_stat;
pub mod segment_kind;
pub mod segment_man_impl;
pub mod segment_man_ops;
pub mod segment_man_support;

#[cfg(test)]
mod tests;

// Re-export all public types so external code using
// `crate::segment::segment_man::X` still works.
pub use peer_stat::{PeerStat, PeerStatus};
pub use segment_kind::SegmentKind;
pub use segment_man_impl::SegmentMan;

// ===========================================================================
// TrackingEntry — Lightweight in-flight segment tracker
// ===========================================================================

/// Lightweight tracking entry for in-flight segments.
///
/// Unlike the C++ version which stores `shared_ptr<Segment>`, this Rust
/// version only stores the CUID and piece index. The actual `SegmentKind`
/// (with the `Piece`) is owned by the caller (download command).
///
/// When the caller needs to cancel or complete a segment, they pass the
/// `SegmentKind` back to `SegmentMan`. For `cancel_segment(cuid)` (which
/// cancels ALL segments for a CUID without the caller passing segments back),
/// we interact with `PieceStorage` by index.
#[derive(Debug, Clone)]
pub(crate) struct TrackingEntry {
    /// Connection ID that owns this segment
    pub(crate) cuid: u64,
    /// Piece index of the in-flight segment
    pub(crate) segment_index: usize,
}
