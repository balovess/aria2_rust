use super::*;
use std::sync::Arc;

#[tokio::test]
async fn test_positioned_write_basic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_basic.bin");

    let mut writer = PositionedDiskWriter::new(&path, Some(1024));
    writer.open().await.unwrap();
    writer.write_at(0, b"hello world").await.unwrap();
    writer.flush().await.unwrap();

    let mut buf = [0u8; 11];
    let n = writer.read_at(0, &mut buf).await.unwrap();
    assert_eq!(n, 11);
    assert_eq!(&buf, b"hello world");
}

#[tokio::test]
async fn test_positioned_write_at_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_offset.bin");

    let mut writer = PositionedDiskWriter::new(&path, None);
    writer.open().await.unwrap();
    // "data at 100" is 11 bytes
    writer.write_at(100, b"data at 100").await.unwrap();
    writer.flush().await.unwrap();

    // Read back at offset 100
    let mut buf = [0u8; 11];
    let n = writer.read_at(100, &mut buf).await.unwrap();
    assert_eq!(n, 11);
    assert_eq!(&buf, b"data at 100");

    // Verify offset 0 is zero-filled (sparse hole / OS zero-fill on extend)
    let mut buf0 = [0xFFu8; 12];
    let n0 = writer.read_at(0, &mut buf0).await.unwrap();
    assert_eq!(n0, 12, "should read full 12 bytes from zero-filled region");
    assert!(
        buf0.iter().all(|&b| b == 0),
        "offset 0 should be zero-filled, got {:?}",
        buf0
    );
}

#[tokio::test]
async fn test_positioned_writer_truncate_and_len() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_trunc.bin");

    let mut writer = PositionedDiskWriter::new(&path, Some(2048));
    writer.open().await.unwrap();

    // Pre-allocated to total_size
    let len = writer.len().await.unwrap();
    assert_eq!(len, 2048);

    writer.truncate(512).await.unwrap();
    let len = writer.len().await.unwrap();
    assert_eq!(len, 512);
}

#[tokio::test]
async fn test_positioned_writer_len_before_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_len_before_open.bin");

    let writer = PositionedDiskWriter::new(&path, Some(9999));
    let len = writer.len().await.unwrap();
    assert_eq!(len, 9999, "should return total_size before open");
}

#[tokio::test]
async fn test_positioned_writer_len_no_total_size_before_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_len_none.bin");

    let writer = PositionedDiskWriter::new(&path, None);
    let len = writer.len().await.unwrap();
    assert_eq!(len, 0, "should return 0 before open when no total_size");
}

#[tokio::test]
async fn test_positioned_writer_resume_does_not_truncate() {
    // Verify that opening an existing file with total_size does NOT truncate
    // existing data — critical for resume scenarios.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_resume.bin");

    // First writer: create and write data
    {
        let mut w = PositionedDiskWriter::new(&path, Some(1024));
        w.open().await.unwrap();
        w.write_at(0, b"resume-data").await.unwrap();
        w.flush().await.unwrap();
    }

    // Second writer: open existing file with same total_size
    {
        let mut w = PositionedDiskWriter::new(&path, Some(1024));
        w.open().await.unwrap();
        let mut buf = [0u8; 11];
        let n = w.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"resume-data", "existing data must survive reopen");
    }
}

#[tokio::test]
async fn test_positioned_writer_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("deep").join("file.bin");

    let mut writer = PositionedDiskWriter::new(&path, Some(64));
    writer.open().await.unwrap();
    writer.write_at(0, b"x").await.unwrap();
    writer.flush().await.unwrap();

    assert!(path.exists(), "file should be created with parent dirs");
}

#[tokio::test]
async fn test_concurrent_writes_non_overlapping() {
    // Test with a shared writer wrapped in Arc<tokio::sync::Mutex<>>.
    // The internal std::sync::Mutex is held only for the syscall
    // (microseconds), so even with the outer serialization the test
    // validates positioned-write correctness and data integrity.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_concurrent_shared.bin");

    let chunk_size: usize = 64 * 1024;
    let num_tasks: usize = 4;

    let mut writer = PositionedDiskWriter::new(&path, Some((chunk_size * num_tasks) as u64));
    writer.open().await.unwrap();
    let writer = Arc::new(tokio::sync::Mutex::new(writer));

    let mut handles = Vec::with_capacity(num_tasks);
    for i in 0..num_tasks {
        let offset = (i as u64) * chunk_size as u64;
        let fill = (i as u8) + 1;
        let data = bytes::Bytes::from(vec![fill; chunk_size]);
        let w = writer.clone();
        handles.push(tokio::spawn(async move {
            let mut guard = w.lock().await;
            guard.write_bytes_at(offset, data).await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    {
        let mut guard = writer.lock().await;
        guard.flush().await.unwrap();
    }

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), chunk_size * num_tasks);
    for i in 0..num_tasks {
        let start = i * chunk_size;
        let expected = (i as u8) + 1;
        let chunk = &content[start..start + chunk_size];
        assert!(
            chunk.iter().all(|&b| b == expected),
            "data mismatch in task {} chunk",
            i
        );
    }
}

#[tokio::test]
async fn test_concurrent_writes_separate_writers() {
    // True OS-level concurrency: each task opens its OWN writer to the SAME
    // file path and writes to non-overlapping offsets. pwrite is atomic and
    // offset-based, so concurrent non-overlapping writes are safe.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_concurrent_sep.bin");

    let chunk_size: usize = 64 * 1024;
    let num_tasks: usize = 4;

    // Pre-create and allocate the file via one writer, then drop it.
    {
        let mut w0 = PositionedDiskWriter::new(&path, Some((chunk_size * num_tasks) as u64));
        w0.open().await.unwrap();
        w0.flush().await.unwrap();
    }

    let mut handles = Vec::with_capacity(num_tasks);
    for i in 0..num_tasks {
        let offset = (i as u64) * chunk_size as u64;
        let fill = (i as u8) + 1;
        let data = vec![fill; chunk_size];
        let path_clone = path.clone();
        handles.push(tokio::spawn(async move {
            let mut w = PositionedDiskWriter::new(&path_clone, None);
            w.open().await.unwrap();
            w.write_at(offset, &data).await.unwrap();
            w.flush().await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), chunk_size * num_tasks);
    for i in 0..num_tasks {
        let start = i * chunk_size;
        let expected = (i as u8) + 1;
        let chunk = &content[start..start + chunk_size];
        assert!(
            chunk.iter().all(|&b| b == expected),
            "data mismatch in separate-writer task {} chunk",
            i
        );
    }
}

#[tokio::test]
async fn test_positioned_writer_write_bytes_at_zero_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_zero_copy.bin");

    let mut writer = PositionedDiskWriter::new(&path, Some(256));
    writer.open().await.unwrap();

    let data = bytes::Bytes::from(vec![0xAB; 128]);
    writer.write_bytes_at(0, data).await.unwrap();
    writer.flush().await.unwrap();

    let mut buf = [0u8; 128];
    let n = writer.read_at(0, &mut buf).await.unwrap();
    assert_eq!(n, 128);
    assert!(buf.iter().all(|&b| b == 0xAB));
}
