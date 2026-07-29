//! io_uring tests (Linux + feature only)
//!
//! These tests are excluded on Windows/macOS and when the `io_uring` feature is
//! off. They use `tokio_uring::start` to drive the io_uring runtime.

use super::io_uring::IoUringDiskWriter;
use crate::filesystem::disk_writer::SeekableDiskWriter;

#[test]
fn test_iouring_basic_write_read() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iouring_basic.bin");

        let mut writer = IoUringDiskWriter::new(&path, Some(1024));
        writer.open().await.unwrap();
        writer.write_at(0, b"hello io_uring").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 14];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 14);
        assert_eq!(&buf, b"hello io_uring");
    });
}

#[test]
fn test_iouring_write_at_offset() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iouring_offset.bin");

        let mut writer = IoUringDiskWriter::new(&path, None);
        writer.open().await.unwrap();
        writer.write_at(100, b"offset data").await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 11];
        let n = writer.read_at(100, &mut buf).await.unwrap();
        assert_eq!(n, 11);
        assert_eq!(&buf, b"offset data");
    });
}

#[test]
fn test_iouring_truncate_and_len() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iouring_trunc.bin");

        let mut writer = IoUringDiskWriter::new(&path, Some(2048));
        writer.open().await.unwrap();

        let len = writer.len().await.unwrap();
        assert_eq!(len, 2048);

        writer.truncate(512).await.unwrap();
        let len = writer.len().await.unwrap();
        assert_eq!(len, 512);
    });
}

#[test]
fn test_iouring_close_reopen() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iouring_close.bin");

        let mut writer = IoUringDiskWriter::new(&path, None);
        writer.open().await.unwrap();
        writer.write_at(0, b"before close").await.unwrap();
        writer.flush().await.unwrap();
        writer.close().await.unwrap();

        writer.open().await.unwrap();
        writer.write_at(12, b" after reopen").await.unwrap();
        writer.flush().await.unwrap();
        writer.close().await.unwrap();

        let content = std::fs::read(&path).unwrap();
        assert_eq!(&content, b"before close after reopen");
    });
}

#[test]
fn test_iouring_resume_does_not_truncate() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iouring_resume.bin");

        // First writer: create and write data
        {
            let mut w = IoUringDiskWriter::new(&path, Some(1024));
            w.open().await.unwrap();
            w.write_at(0, b"resume-data").await.unwrap();
            w.flush().await.unwrap();
            w.close().await.unwrap();
        }

        // Second writer: open existing file with same total_size
        {
            let mut w = IoUringDiskWriter::new(&path, Some(1024));
            w.open().await.unwrap();
            let mut buf = [0u8; 11];
            let n = w.read_at(0, &mut buf).await.unwrap();
            assert_eq!(n, 11);
            assert_eq!(&buf, b"resume-data", "existing data must survive reopen");
        }
    });
}

#[test]
fn test_iouring_creates_parent_dirs() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("file.bin");

        let mut writer = IoUringDiskWriter::new(&path, Some(64));
        writer.open().await.unwrap();
        writer.write_at(0, b"x").await.unwrap();
        writer.flush().await.unwrap();

        assert!(path.exists(), "file should be created with parent dirs");
    });
}

#[test]
fn test_iouring_write_bytes_at() {
    tokio_uring::start(async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iouring_bytes.bin");

        let mut writer = IoUringDiskWriter::new(&path, Some(256));
        writer.open().await.unwrap();

        let data = bytes::Bytes::from(vec![0xAB; 128]);
        writer.write_bytes_at(0, data).await.unwrap();
        writer.flush().await.unwrap();

        let mut buf = [0u8; 128];
        let n = writer.read_at(0, &mut buf).await.unwrap();
        assert_eq!(n, 128);
        assert!(buf.iter().all(|&b| b == 0xAB));
    });
}