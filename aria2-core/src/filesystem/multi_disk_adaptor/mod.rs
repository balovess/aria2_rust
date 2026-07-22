//! Multi-file disk adaptor that maps a contiguous torrent byte stream to
//! individual files on disk.
//!
//! This is the Rust equivalent of the C++ aria2 `MultiDiskAdaptor` class.
//! It handles cross-file writes/reads, shared-piece analysis, lazy file
//! opening, and max-open-files eviction.

mod adaptor;
mod disk_writer_entry;
mod file_entry;

#[cfg(test)]
mod tests;

pub use adaptor::MultiDiskAdaptor;
pub use disk_writer_entry::DiskWriterEntry;
pub use file_entry::FileEntry;
