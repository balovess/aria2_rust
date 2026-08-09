//! BitfieldMan — Manages piece-level bitfields.
//!
//! This is the Rust equivalent of the C++ `BitfieldMan` class.
//! It tracks three bitfields:
//! - **completion**: which pieces have been fully downloaded
//! - **use**: which pieces are currently being downloaded (in-flight)
//! - **filter**: which pieces are filtered out (not to be downloaded)

mod core;
mod filter;
mod helpers;
mod query;
mod selection;

// Public API — preserve the original `bitfield_man::BitfieldMan` surface.
pub use core::BitfieldMan;
