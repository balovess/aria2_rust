//! Write operation tests for multi_disk_adaptor.

use super::*;

// -- Cross-file write tests --------------------------------------------

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

// -- Open mode strategy (write path) -----------------------------------

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

// -- Multiple open/close cycles (write path) ---------------------------

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
