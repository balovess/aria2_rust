//! DefaultPieceStorage — Default implementation of PieceStorage.
//!
//! Uses BitfieldMan for piece tracking and supports piece selection strategies.
//! Mirrors C++ DefaultPieceStorage.

mod struct_def;
mod piece_ops;

pub use struct_def::DefaultPieceStorage;
