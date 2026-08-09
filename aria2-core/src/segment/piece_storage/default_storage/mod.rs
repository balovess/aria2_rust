//! DefaultPieceStorage — Default implementation of PieceStorage.
//!
//! Uses BitfieldMan for piece tracking and supports piece selection strategies.
//! Mirrors C++ DefaultPieceStorage.

mod piece_ops;
mod struct_def;

pub use struct_def::DefaultPieceStorage;
