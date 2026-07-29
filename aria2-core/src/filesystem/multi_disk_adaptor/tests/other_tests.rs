//! Remaining tests for multi_disk_adaptor (binary search, file existence,
//! eviction, read-only, close, trait, and flush).

use std::path::Path;

use crate::filesystem::disk_adaptor::DiskAdaptor;

use super::*;

// -- Binary search tests -----------------------------------------------

#[test]
fn test_find_first_entry_index_basic() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("a"), 10, 0, true),
        FileEntry::new(dir.path().join("b"), 10, 10, true),
        FileEntry::new(dir.path().join("c"), 10, 20, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);

    assert_eq!(adaptor.find_first_entry_index(0).unwrap(), 0);
    assert_eq!(adaptor.find_first_entry_index(5).unwrap(), 0);
    assert_eq!(adaptor.find_first_entry_index(9).unwrap(), 0);
    assert_eq!(adaptor.find_first_entry_index(10).unwrap(), 1);
    assert_eq!(adaptor.find_first_entry_index(15).unwrap(), 1);
    assert_eq!(adaptor.find_first_entry_index(19).unwrap(), 1);
    assert_eq!(adaptor.find_first_entry_index(20).unwrap(), 2);
    assert_eq!(adaptor.find_first_entry_index(29).unwrap(), 2);
}

#[test]
fn test_find_first_entry_index_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("a"), 10, 0, true),
        FileEntry::new(dir.path().join("b"), 10, 10, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);

    // Past the end of all files
    assert!(adaptor.find_first_entry_index(20).is_err());
}

#[test]
fn test_find_first_entry_index_empty() {
    let adaptor = MultiDiskAdaptor::new(1);
    assert!(adaptor.find_first_entry_index(0).is_err());
}

#[test]
fn test_find_first_entry_index_zero_length_files() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("a"), 0, 0, true),
        FileEntry::new(dir.path().join("b"), 5, 0, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);

    // offset 0 falls in file b (len=5, off=0), not file a (len=0)
    assert_eq!(adaptor.find_first_entry_index(0).unwrap(), 1);
    assert_eq!(adaptor.find_first_entry_index(4).unwrap(), 1);
    assert!(adaptor.find_first_entry_index(5).is_err());
}

// -- File existence test -----------------------------------------------

#[tokio::test]
async fn test_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    tokio::fs::write(dir.path().join("file1.txt"), b"hello")
        .await
        .unwrap();

    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 5, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 5, 5, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);

    // file1 exists, file2 doesn't
    assert!(adaptor.file_exists());
}

// -- cutTrailingGarbage test -------------------------------------------

#[tokio::test]
async fn test_cut_trailing_garbage() {
    let dir = tempfile::tempdir().unwrap();

    // Create files that are 100 bytes larger than declared
    tokio::fs::write(dir.path().join("file1.txt"), vec![0u8; 356])
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("file2.txt"), vec![0u8; 612])
        .await
        .unwrap();

    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 256, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 512, 256, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    adaptor.cut_trailing_garbage().await.unwrap();

    // Verify files are truncated to declared lengths
    let meta1 = tokio::fs::metadata(dir.path().join("file1.txt"))
        .await
        .unwrap();
    assert_eq!(meta1.len(), 256);

    let meta2 = tokio::fs::metadata(dir.path().join("file2.txt"))
        .await
        .unwrap();
    assert_eq!(meta2.len(), 512);

    adaptor.close_file().await;
}

// -- tryCloseFile eviction test ----------------------------------------

#[tokio::test]
async fn test_try_close_file() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 10, 10, true),
        FileEntry::new(dir.path().join("file3.txt"), 10, 20, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // All 3 files should be open
    assert_eq!(adaptor.opened_entries.len(), 3);

    // Close 2 files
    let closed = adaptor.try_close_file(2).await;
    assert_eq!(closed, 2);
    assert_eq!(adaptor.opened_entries.len(), 1);

    // Close more than available
    let closed = adaptor.try_close_file(5).await;
    assert_eq!(closed, 1); // Only 1 was left
    assert!(adaptor.opened_entries.is_empty());
}

// -- Read-only mode test -----------------------------------------------

#[test]
fn test_read_only_toggle() {
    let mut adaptor = MultiDiskAdaptor::new(512);
    assert!(!adaptor.is_read_only_enabled());

    adaptor.enable_read_only();
    assert!(adaptor.is_read_only_enabled());

    adaptor.disable_read_only();
    assert!(!adaptor.is_read_only_enabled());
}

// -- Close all files test ----------------------------------------------

#[tokio::test]
async fn test_close_all_files() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 10, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 10, 10, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    assert!(!adaptor.opened_entries.is_empty());

    adaptor.close_file().await;
    assert!(adaptor.opened_entries.is_empty());
}

// -- DiskAdaptor trait tests -------------------------------------------

#[tokio::test]
async fn test_disk_adaptor_trait_open() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);

    // DiskAdaptor::open delegates to open_file
    adaptor.open(Path::new("ignored")).await.unwrap();

    adaptor.write(0, b"test data!").await.unwrap();
    adaptor.close().await.unwrap();

    let content = tokio::fs::read_to_string(dir.path().join("file1.txt"))
        .await
        .unwrap();
    assert!(content.starts_with("test data!"));
}

#[tokio::test]
async fn test_disk_adaptor_trait_read_write() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 10, 15, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open(Path::new("ignored")).await.unwrap();

    // Write 16 bytes starting at offset 5: file1[5..15] (10 bytes) + file2[0..6] (6 bytes)
    adaptor.write(5, b"1234567890ABCDEF").await.unwrap();
    adaptor.flush().await.unwrap();

    // Read back via trait method
    let data = adaptor.read(5, 16).await.unwrap();
    assert_eq!(&data[..16], b"1234567890ABCDEF");

    adaptor.close().await.unwrap();
}

#[tokio::test]
async fn test_disk_adaptor_trait_truncate_error() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open(Path::new("ignored")).await.unwrap();

    // truncate should return error for MultiDiskAdaptor
    assert!(adaptor.truncate(100).await.is_err());

    adaptor.close().await.unwrap();
}

#[tokio::test]
async fn test_disk_adaptor_trait_size() {
    let dir = tempfile::tempdir().unwrap();

    tokio::fs::write(dir.path().join("file1.txt"), vec![0u8; 10])
        .await
        .unwrap();

    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open(Path::new("ignored")).await.unwrap();

    let size = adaptor.size().await.unwrap();
    assert_eq!(size, 10);

    adaptor.close().await.unwrap();
}

// -- Max open files eviction test --------------------------------------

#[tokio::test]
async fn test_max_open_files_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let entries: Vec<FileEntry> = (0..5)
        .map(|i| FileEntry::new(dir.path().join(format!("file{}.txt", i)), 10, i * 10, true))
        .collect();

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_max_open_files(3);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Only 3 should be open (5 files, max 3)
    assert!(adaptor.opened_entries.len() <= 3);

    // Writing should still work even with eviction
    adaptor.write_data(0, b"0123456789").await.unwrap();
    adaptor.write_data(40, b"0123456789").await.unwrap();

    adaptor.close_file().await;
}

// -- Flush OS buffers test ---------------------------------------------

#[tokio::test]
async fn test_flush_os_buffers() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    adaptor.write_data(0, b"0123456789").await.unwrap();
    adaptor.flush_os_buffers().await.unwrap();

    adaptor.close_file().await;

    // Verify data persisted after flush
    let content = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(&content[..10], b"0123456789");
}
