#![allow(clippy::empty_line_after_doc_comments)]

//! Manages piece/block download operations for BitTorrent downloads.
//!
//! This module encapsulates:
//! - Piece selection and block request logic
//! - Data verification and hash checking
//! - Writing downloaded data to disk or multi-file layouts
//! - File-backed piece provider for seeding phase
//!
//! Extracted from BtDownloadCommand to follow single responsibility principle,
//! mirroring original aria2 C++ architecture separation.

mod file_backed_provider;
mod multi_file_writer;
mod piece_download_state;

#[cfg(test)]
mod tests;

// Public re-exports — all items remain accessible at the same paths.
pub use file_backed_provider::FileBackedPieceProvider;
pub use multi_file_writer::{
    write_piece_to_multi_files, write_piece_to_multi_files_coalesced,
    write_piece_to_multi_files_coalesced_with_limit,
};
pub use piece_download_state::PieceDownloadState;
