//! Bitfield type re-exports and shared utilities.
//!
//! Re-exports the [`Bitfield`] type from `aria2-protocol` and delegates
//! to [`bitfield_util::test_bit`] for MSB-first bit testing.

pub use aria2_protocol::bittorrent::piece::bitfield::Bitfield;

// Re-export test_bit from the canonical bitfield_util module
pub use super::bitfield_util::test_bit;
