//! Piece tracking for segmented downloads.
//!
//! Implements the aria2-compatible `Piece` struct for block-level completion
//! tracking with dual bitfields (completed + in-use), user reference counting,
//! and hash verification.
//!
//! Rust equivalent of the C++ aria2 `Piece` class. Key differences:
//! - `get_missing_unused_block_index` takes `&mut self` instead of `const` + `mutable`
//! - Hash context uses an enum for static dispatch instead of runtime polymorphism
//! - Uses a self-contained bitfield instead of the aria2-protocol Bitfield

mod bitfield;
mod completion;
mod piece_impl;
#[cfg(test)]
mod tests;
mod traits;

pub use piece_impl::{Piece, DEFAULT_BLOCK_LENGTH};
