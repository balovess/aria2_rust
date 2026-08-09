//! Test infrastructure, shared helpers, and sub-module re-exports for
//! `multi_disk_adaptor` tests.

use std::path::{Path, PathBuf};

use super::*;

mod allocation_tests;
mod other_tests;
mod read_tests;
mod write_tests;

// -- Helper: create temp file entries ----------------------------------

/// Build the canonical C++ test layout (piece_length=2).
pub(crate) fn make_test_entries(dir: &Path) -> Vec<FileEntry> {
    vec![
        FileEntry::new(dir.join("file0.txt"), 0, 0, true),
        FileEntry::new(dir.join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.join("file2.txt"), 7, 15, true),
        FileEntry::new(dir.join("file3.txt"), 0, 22, true),
        FileEntry::new(dir.join("file4.txt"), 2, 22, true),
        FileEntry::new(dir.join("file5.txt"), 0, 24, true),
        FileEntry::new(dir.join("file6.txt"), 3, 24, true),
        FileEntry::new(dir.join("file7.txt"), 0, 27, true),
        FileEntry::new(dir.join("file8.txt"), 2, 27, true),
    ]
}

// -- FileEntry unit tests ----------------------------------------------

#[test]
fn test_file_entry_basic() {
    let fe = FileEntry::new(PathBuf::from("/tmp/test.txt"), 1024, 2048, true);
    assert_eq!(fe.get_path(), Path::new("/tmp/test.txt"));
    assert_eq!(fe.get_length(), 1024);
    assert_eq!(fe.get_offset(), 2048);
    assert_eq!(fe.get_last_offset(), 3072);
    assert!(fe.is_requested());
}

#[test]
fn test_file_entry_not_requested() {
    let fe = FileEntry::new(PathBuf::from("/tmp/skip.txt"), 100, 0, false);
    assert!(!fe.is_requested());
}

#[test]
fn test_file_entry_zero_length() {
    let fe = FileEntry::new(PathBuf::from("/tmp/empty.txt"), 0, 50, true);
    assert_eq!(fe.get_length(), 0);
    assert_eq!(fe.get_offset(), 50);
    assert_eq!(fe.get_last_offset(), 50);
}

#[test]
fn test_file_entry_set_requested() {
    let mut fe = FileEntry::new(PathBuf::from("/tmp/test.txt"), 100, 0, true);
    fe.set_requested(false);
    assert!(!fe.is_requested());
}
