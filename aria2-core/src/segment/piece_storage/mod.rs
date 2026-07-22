//! Piece storage management for segmented downloads.
//!
//! This module provides the [`PieceStorage`] trait and [`DefaultPieceStorage`]
//! implementation for tracking download progress at the piece level. It supports
//! both HTTP segmented downloads and BitTorrent piece-based downloads.
//!
//! # Architecture Reference
//!
//! Based on original aria2 C++ structure:
//! - `src/PieceStorage.h` — Piece storage interface
//! - `src/DefaultPieceStorage.h/.cc` — Default implementation
//! - `src/Piece.h` — Piece class
//! - `src/BitfieldMan.h` — Bitfield management
//!
//! # Key Types
//!
//! - [`BitfieldMan`] — Manages completion/usage/filter bitfields for piece tracking
//! - [`Piece`] — Represents a single downloadable piece with block-level tracking
//! - [`PieceStorage`] — Trait interface for piece storage operations
//! - [`DefaultPieceStorage`] — Default implementation suitable for HTTP/FTP and BT

mod bitfield_man;
mod bt_piece_provider;
mod default_storage;
mod trait_def;
mod types;

#[cfg(test)]
mod tests;

// Public re-exports — preserve the original `piece_storage::X` API surface
pub use bitfield_man::BitfieldMan;
pub use default_storage::DefaultPieceStorage;
pub use trait_def::PieceStorage;
pub use types::StreamPieceSelectorKind;
