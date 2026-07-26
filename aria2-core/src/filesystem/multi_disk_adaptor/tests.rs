//! Unit tests for `multi_disk_adaptor`.

use std::path::{Path, PathBuf};

use crate::filesystem::disk_adaptor::DiskAdaptor;

use super::*;

// ── Helper: create temp file entries ─────────────────────────────

fn make_test_entries(dir: &Path) -> Vec<FileEntry> {
    // C++ test layout (piece_length=2):
    //           1    1    2    2    3
    // 0....5....0....5....0....5....0
    // ++--++--++--++--++--++--++--++--
    // | file0 (len=0, off=0)
    // *************** file1 (len=15, off=0)
    //                ******* file2 (len=7, off=15)
    //                       | file3 (len=0, off=22)
    //                       ** file4 (len=2, off=22)
    //                         | file5 (len=0, off=24)
    //                         *** file6 (len=3, off=24)
    //                            | file7 (len=0, off=27)
    //                            ** file8 (len=2, off=27)
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

// ── FileEntry tests ──────────────────────────────────────────────

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

// ── Construction & shared-piece analysis ─────────────────────────

#[test]
fn test_construction_with_piece_length() {
    let adaptor = MultiDiskAdaptor::new(512);
    assert_eq!(adaptor.get_piece_length(), 512);
    assert!(!adaptor.is_read_only_enabled());
    assert!(adaptor.get_disk_writer_entries().is_empty());
}

#[tokio::test]
async fn test_reset_disk_writer_entries_all_requested() {
    let dir = tempfile::tempdir().unwrap();
    let entries = make_test_entries(dir.path());

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    // All entries should have disk writers since all are requested
    // (or are zero-length at the same offset as a requested file).
    assert!(dw_entries[0].has_disk_writer());
    assert!(dw_entries[1].has_disk_writer());
    assert!(dw_entries[2].has_disk_writer());
    assert!(dw_entries[3].has_disk_writer());
    assert!(dw_entries[4].has_disk_writer());
    assert!(dw_entries[5].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_file0_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    entries[0].set_requested(false);

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    // file0 is not requested but file1 shares a piece with it.
    assert!(dw_entries[0].has_disk_writer());
    assert!(dw_entries[1].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_file0_file1_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    entries[0].set_requested(false);
    entries[1].set_requested(false);

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    // file0: not requested, no piece sharing -> no disk writer
    assert!(!dw_entries[0].has_disk_writer());
    // file1: not requested but file2 spans into it -> needs file allocation
    assert!(dw_entries[1].has_disk_writer());
    assert!(dw_entries[1].needs_file_allocation());
    assert!(dw_entries[2].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_file3_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    entries[3].set_requested(false);

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    // file3 not requested but file4 spans -> needs file allocation
    assert!(dw_entries[3].has_disk_writer());
    assert!(dw_entries[3].needs_file_allocation());
    assert!(dw_entries[4].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_file4_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    entries[4].set_requested(false);

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    // file3 is zero-length, no overlap with file4
    assert!(!dw_entries[4].has_disk_writer());
    assert!(dw_entries[5].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_file3_file4_not_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    entries[3].set_requested(false);
    entries[4].set_requested(false);

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    assert!(!dw_entries[3].has_disk_writer());
    assert!(!dw_entries[4].has_disk_writer());
    assert!(dw_entries[5].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_only_first_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    for i in 1..9 {
        entries[i].set_requested(false);
    }

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    assert!(dw_entries[0].has_disk_writer());
    assert!(!dw_entries[1].has_disk_writer());
    assert!(!dw_entries[2].has_disk_writer());
    assert!(!dw_entries[3].has_disk_writer());
    assert!(!dw_entries[4].has_disk_writer());
    assert!(!dw_entries[5].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_first_two_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    for i in 2..9 {
        entries[i].set_requested(false);
    }

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    assert!(dw_entries[0].has_disk_writer());
    assert!(dw_entries[1].has_disk_writer());
    // file1 spans into file2
    assert!(dw_entries[2].has_disk_writer());
    assert!(!dw_entries[2].needs_file_allocation());
    assert!(!dw_entries[3].has_disk_writer());
    assert!(!dw_entries[4].has_disk_writer());
    assert!(!dw_entries[5].has_disk_writer());

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_reset_disk_writer_entries_only_file6_requested() {
    let dir = tempfile::tempdir().unwrap();
    let mut entries = make_test_entries(dir.path());
    for i in 0..6 {
        entries[i].set_requested(false);
    }
    entries[8].set_requested(false);

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let dw_entries = adaptor.get_disk_writer_entries();
    assert!(!dw_entries[0].has_disk_writer());
    assert!(!dw_entries[1].has_disk_writer());
    assert!(!dw_entries[2].has_disk_writer());
    assert!(!dw_entries[3].has_disk_writer());
    assert!(!dw_entries[4].has_disk_writer());
    // file6 spans file5 (backward scan) and file8 (forward scan)
    assert!(dw_entries[5].has_disk_writer());
    assert!(dw_entries[6].has_disk_writer());
    assert!(dw_entries[7].has_disk_writer());
    assert!(dw_entries[8].has_disk_writer());

    adaptor.close_file().await;
}

// ── Binary search tests ──────────────────────────────────────────

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

// ── Cross-file write tests ───────────────────────────────────────

#[tokio::test]
async fn test_cross_file_write_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Write "12345" at offset 0 (within file1 only)
    adaptor.write_data(0, b"12345").await.unwrap();

    adaptor.close_file().await;

    // Verify file1.txt contains "12345"
    let content = tokio::fs::read_to_string(dir.path().join("file1.txt"))
        .await
        .unwrap();
    assert_eq!(&content[..5], "12345");
}

#[tokio::test]
async fn test_cross_file_write_spanning_two_files() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Write at offset 5, length 11: spans file1[5..15] and file2[0..1]
    adaptor.write_data(5, b"67890ABCDEF").await.unwrap();

    adaptor.close_file().await;

    // file1.txt should have 15 bytes (first 5 uninitialized, next 10 from write)
    let content1 = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(content1.len(), 15);
    assert_eq!(&content1[5..15], b"67890ABCDE");

    // file2.txt first byte should be 'F'
    let content2 = tokio::fs::read(dir.path().join("file2.txt")).await.unwrap();
    assert!(content2.len() >= 1);
    assert_eq!(content2[0], b'F');
}

#[tokio::test]
async fn test_cross_file_write_full_cplusplus_test() {
    // Reproduces the C++ testWriteData test case.
    let dir = tempfile::tempdir().unwrap();
    let entries = make_test_entries(dir.path());

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Write "12345" at offset 0
    adaptor.write_data(0, b"12345").await.unwrap();
    adaptor.close_file().await;

    // Verify file1.txt
    let content1 = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(&content1[..5], b"12345");

    // Re-open and write "67890ABCDEF" at offset 5
    adaptor.open_file().await.unwrap();
    adaptor.write_data(5, b"67890ABCDEF").await.unwrap();
    adaptor.close_file().await;

    let content1 = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(content1.len(), 15);
    assert_eq!(&content1[..15], b"1234567890ABCDE");

    let content2 = tokio::fs::read(dir.path().join("file2.txt")).await.unwrap();
    assert!(content2.len() >= 1);
    assert_eq!(content2[0], b'F');

    // Re-open and write "12345123456712" at offset 10
    adaptor.open_file().await.unwrap();
    adaptor.write_data(10, b"12345123456712").await.unwrap();
    adaptor.close_file().await;

    let content1 = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(&content1[..15], b"123456789012345");

    let content2 = tokio::fs::read(dir.path().join("file2.txt")).await.unwrap();
    assert_eq!(content2.len(), 7);
    assert_eq!(&content2[..7], b"1234567");
}

// ── Cross-file read tests ────────────────────────────────────────

#[tokio::test]
async fn test_cross_file_read_single_file() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
    ];

    // Write test data first
    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    adaptor.write_data(0, b"1234567890ABCDE").await.unwrap();
    adaptor.write_data(15, b"FGHIJKLM").await.unwrap();
    adaptor.flush_os_buffers().await.unwrap();

    // Read from offset 0, length 15 (within file1)
    let data = adaptor.read_data(0, 15).await.unwrap();
    assert_eq!(&data[..15], b"1234567890ABCDE");

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_cross_file_read_spanning_files() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    adaptor.write_data(0, b"1234567890ABCDE").await.unwrap();
    adaptor.write_data(15, b"FGHIJKLM").await.unwrap();
    adaptor.flush_os_buffers().await.unwrap();

    // Read from offset 6, length 10: spans file1[6..15] and file2[0..1]
    let data = adaptor.read_data(6, 10).await.unwrap();
    assert_eq!(&data[..10], b"7890ABCDEF");

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_cross_file_read_across_three_files() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
        FileEntry::new(dir.path().join("file3.txt"), 3, 22, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Write data across all three files in one operation.
    // 25 bytes total: file1[0..15] + file2[0..7] + file3[0..3]
    adaptor
        .write_data(0, b"1234567890ABCDEFGHIJKLNOP")
        .await
        .unwrap();
    adaptor.flush_os_buffers().await.unwrap();

    // Read from offset 20, length 4: spans file2[5..7] and file3[0..2]
    // file2 = "FGHIJKL" -> file2[5..7] = "KL"
    // file3 = "NOP" -> file3[0..2] = "NO"
    let data = adaptor.read_data(20, 4).await.unwrap();
    assert_eq!(&data[..4], b"KLNO");

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_cross_file_read_full_stream() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 15, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 7, 15, true),
        FileEntry::new(dir.path().join("file3.txt"), 3, 22, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Write 25 bytes across all files in one operation:
    // file1[0..15] = "1234567890ABCDE"
    // file2[0..7]  = "FGHIJKL"
    // file3[0..3]  = "NOP"
    adaptor
        .write_data(0, b"1234567890ABCDEFGHIJKLNOP")
        .await
        .unwrap();
    adaptor.flush_os_buffers().await.unwrap();

    // Read entire stream (25 bytes)
    let data = adaptor.read_data(0, 25).await.unwrap();
    assert_eq!(&data[..25], b"1234567890ABCDEFGHIJKLNOP");

    adaptor.close_file().await;
}

// ── Lazy file opening tests ──────────────────────────────────────

#[tokio::test]
async fn test_open_existing_file_lazy() {
    let dir = tempfile::tempdir().unwrap();

    // Create files on disk first
    tokio::fs::write(dir.path().join("file1.txt"), b"existing data here!")
        .await
        .unwrap();

    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 19, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(2);
    adaptor.set_file_entries(entries);
    adaptor.open_existing_file().await.unwrap();

    // No files should be open yet
    assert!(adaptor.opened_entries.is_empty());

    // But reading should work (lazy open)
    // "existing" is 8 bytes
    let data = adaptor.read_data(0, 8).await.unwrap();
    assert_eq!(&data[..8], b"existing");

    adaptor.close_file().await;
}

// ── File existence and size tests ────────────────────────────────

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

#[tokio::test]
async fn test_size_calculation() {
    let dir = tempfile::tempdir().unwrap();

    // Create files with known sizes
    tokio::fs::write(dir.path().join("file1.txt"), vec![0u8; 256])
        .await
        .unwrap();
    tokio::fs::write(dir.path().join("file2.txt"), vec![0u8; 512])
        .await
        .unwrap();

    let entries = vec![
        FileEntry::new(dir.path().join("file1.txt"), 256, 0, true),
        FileEntry::new(dir.path().join("file2.txt"), 512, 256, true),
    ];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    let size = adaptor.size().await.unwrap();
    assert_eq!(size, 768);

    adaptor.close_file().await;
}

// ── cutTrailingGarbage tests ─────────────────────────────────────

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

// ── tryCloseFile eviction tests ──────────────────────────────────

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

// ── Read-only mode tests ─────────────────────────────────────────

#[test]
fn test_read_only_toggle() {
    let mut adaptor = MultiDiskAdaptor::new(512);
    assert!(!adaptor.is_read_only_enabled());

    adaptor.enable_read_only();
    assert!(adaptor.is_read_only_enabled());

    adaptor.disable_read_only();
    assert!(!adaptor.is_read_only_enabled());
}

// ── Close all files tests ────────────────────────────────────────

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

// ── Open mode strategy tests ─────────────────────────────────────

#[tokio::test]
async fn test_init_and_open_truncates() {
    let dir = tempfile::tempdir().unwrap();

    // Create file with existing content
    tokio::fs::write(dir.path().join("file1.txt"), b"existing content here")
        .await
        .unwrap();

    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 5, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.init_and_open_file().await.unwrap();

    // Write new data
    adaptor.write_data(0, b"12345").await.unwrap();
    adaptor.close_file().await;

    // File should contain only the new data (truncated)
    let content = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(&content[..5], b"12345");
}

#[tokio::test]
async fn test_open_file_preserves_content() {
    let dir = tempfile::tempdir().unwrap();

    // Create file with existing content (21 bytes)
    tokio::fs::write(dir.path().join("file1.txt"), b"existing content here")
        .await
        .unwrap();

    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 21, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Content should be preserved
    let data = adaptor.read_data(0, 21).await.unwrap();
    assert_eq!(&data[..21], b"existing content here");

    adaptor.close_file().await;
}

// ── DiskAdaptor trait tests ──────────────────────────────────────

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

// ── Max open files eviction test ─────────────────────────────────

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

// ── Empty data edge cases ────────────────────────────────────────

#[tokio::test]
async fn test_write_empty_data() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Writing empty data should succeed without error
    adaptor.write_data(0, b"").await.unwrap();

    adaptor.close_file().await;
}

#[tokio::test]
async fn test_read_zero_length() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);
    adaptor.open_file().await.unwrap();

    // Reading zero bytes should return empty vec
    let data = adaptor.read_data(0, 0).await.unwrap();
    assert!(data.is_empty());

    adaptor.close_file().await;
}

// ── Flush OS buffers test ────────────────────────────────────────

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

// ── Multiple open/close cycles ───────────────────────────────────

#[tokio::test]
async fn test_open_close_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let entries = vec![FileEntry::new(dir.path().join("file1.txt"), 10, 0, true)];

    let mut adaptor = MultiDiskAdaptor::new(1);
    adaptor.set_file_entries(entries);

    for i in 0..3 {
        adaptor.open_file().await.unwrap();
        adaptor.write_data(0, &[i; 10]).await.unwrap();
        adaptor.close_file().await;
    }

    // Last write should have written 0x02
    let content = tokio::fs::read(dir.path().join("file1.txt")).await.unwrap();
    assert_eq!(content[..10], [2u8; 10]);
}
