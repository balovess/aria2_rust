//! Read operation tests for multi_disk_adaptor.

use super::*;

// -- Cross-file read tests ---------------------------------------------

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

// -- Lazy file opening (read path) -------------------------------------

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

// -- Open mode strategy (read path) ------------------------------------

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

// -- Empty data edge case (read path) ----------------------------------

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
