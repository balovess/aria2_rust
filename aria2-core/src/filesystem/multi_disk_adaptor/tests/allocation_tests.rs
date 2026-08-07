//! File allocation / shared-piece analysis tests for multi_disk_adaptor.

use super::*;

// -- Construction & shared-piece analysis --------------------------------

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
    for entry in entries.iter_mut().take(9).skip(1) {
        entry.set_requested(false);
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
    for entry in entries.iter_mut().take(9).skip(2) {
        entry.set_requested(false);
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
    for entry in entries.iter_mut().take(6) {
        entry.set_requested(false);
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

// -- Size calculation ---------------------------------------------------

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
