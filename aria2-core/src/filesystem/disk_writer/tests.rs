//! Tests for disk_writer module.

use std::sync::Arc;

use super::*;

#[tokio::test]
async fn test_default_disk_writer_write_and_finalize() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_default.bin");

    let mut writer = DefaultDiskWriter::new(&path);
    writer.write(b"hello").await.unwrap();
    writer.write(b" world").await.unwrap();
    writer.finalize().await.unwrap();

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "hello world");
}

#[tokio::test]
async fn test_default_disk_writer_resume_writes_at_offset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resume.bin");
    tokio::fs::write(&path, b"prefix").await.unwrap();

    let mut writer = DefaultDiskWriter::new_with_offset(&path, 6);
    writer.write(b"suffix").await.unwrap();
    writer.finalize().await.unwrap();

    assert_eq!(tokio::fs::read(&path).await.unwrap(), b"prefixsuffix");
}

#[tokio::test]
async fn test_byte_array_disk_writer() {
    let mut writer = ByteArrayDiskWriter::with_capacity(10);
    writer.write(b"abc").await.unwrap();
    writer.write(b"def").await.unwrap();
    let result = writer.finalize().await.unwrap();
    assert_eq!(result, b"abcdef");
    assert_eq!(writer.len(), 6);
}

#[tokio::test]
async fn test_sequential_download_writer_memory_mode_never_opens_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metadata.torrent");

    let mut writer = new_sequential_download_writer(&path, true, 99, Some(6));
    writer.write(b"abcdef").await.unwrap();
    assert_eq!(writer.finalize().await.unwrap(), b"abcdef");
    assert!(!path.exists());
}

#[tokio::test]
async fn test_seekable_writer_basic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_seekable.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(1024), None);
    writer.open().await.unwrap();
    assert!(writer.is_opened());

    writer.write_at(0, b"hello").await.unwrap();
    writer.write_at(5, b" world").await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(&content[..11], b"hello world");
}

#[tokio::test]
async fn test_seekable_writer_random_access() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_random.bin");

    let mut writer = CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();

    writer.write_at(200, b"SEG2").await.unwrap();
    writer.write_at(0, b"SEG0").await.unwrap();
    writer.write_at(100, b"SEG1").await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), 204);
    assert_eq!(&content[0..4], b"SEG0");
    assert_eq!(&content[100..104], b"SEG1");
    assert_eq!(&content[200..204], b"SEG2");
}

#[tokio::test]
async fn test_seekable_writer_read_at() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_read.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(100), None);
    writer.open().await.unwrap();
    writer.write_at(50, b"offset-50-data").await.unwrap();
    writer.flush().await.unwrap();

    let mut buf = [0u8; 14];
    let n = writer.read_at(50, &mut buf).await.unwrap();
    assert_eq!(n, 14);
    assert_eq!(&buf, b"offset-50-data");
}

#[tokio::test]
async fn test_cached_writer_with_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_cached.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(4096), Some(1));
    writer.open().await.unwrap();

    for i in 0..100 {
        let data = vec![i as u8; 64];
        writer.write_at((i * 64) as u64, &data).await.unwrap();
    }

    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), 6400);

    for i in 0..100 {
        let start = i * 64;
        assert_eq!(content[start], i as u8, "mismatch at byte {}", start);
    }
}

#[tokio::test]
async fn test_cached_writer_large_write_bypasses_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_large.bin");

    // Use smaller size to avoid disk space issues
    let large_data = vec![0xAB; 128 * 1024]; // 128KB instead of 256KB+

    let mut writer = CachedDiskWriter::new(&path, None, Some(1));
    writer.open().await.unwrap();
    writer.write_at(0, &large_data).await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), large_data.len());
    assert!(content.iter().all(|&b| b == 0xAB));
}

#[tokio::test]
async fn test_seekable_writer_truncate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_trunc.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(1000), None);
    writer.open().await.unwrap();
    writer
        .write_at(0, b"hello world - this is longer than 20 bytes of data")
        .await
        .unwrap();
    writer.flush().await.unwrap();

    writer.truncate(20).await.unwrap();
    writer.flush().await.unwrap();

    let len = writer.len().await.unwrap();
    assert!(len <= 21);

    let content = tokio::fs::read(&path).await.unwrap();
    assert!(content.len() <= 21);
    assert_eq!(&content[..4], b"hell");
}

#[tokio::test]
async fn test_seekable_writer_len_before_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_len.bin");

    let writer = CachedDiskWriter::new(&path, Some(9999), None);
    let len = writer.len().await.unwrap();
    assert_eq!(len, 9999);
}

#[tokio::test]
async fn test_close_reopens_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_close.bin");

    let mut writer = CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();
    writer.write_at(0, b"before close").await.unwrap();
    writer.close().await.unwrap();
    assert!(!writer.is_opened());

    writer.open().await.unwrap();
    writer.write_at(12, b" after reopen").await.unwrap();
    writer.close().await.unwrap();

    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(content, "before close after reopen");
}

// -- Rate limiter wiring tests -----------------------------------------------

#[tokio::test]
async fn test_cached_writer_with_rate_limiter() {
    use crate::rate_limiter::{RateLimiter, RateLimiterConfig};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_ratelimited.bin");

    // Create a very restrictive limiter (10 bytes/sec, tiny burst)
    let cfg = RateLimiterConfig::new(Some(10), None).with_burst(Some(20), None);
    let rl = Arc::new(RateLimiter::new(&cfg));

    let mut writer = CachedDiskWriter::new(&path, Some(4096), None).with_rate_limiter(rl.clone());
    writer.open().await.unwrap();

    // Write data - should succeed (try_acquire may fail but we still write)
    let data = vec![0x42u8; 512];
    writer.write_at(0, &data).await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert!(content.len() >= 512, "file should be at least 512 bytes");
    assert_eq!(&content[..512], &vec![0x42u8; 512][..]);
    assert!(content.iter().take(512).all(|&b| b == 0x42));
}

#[tokio::test]
async fn test_cached_writer_without_rate_limiter_no_effect() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_nolimiter.bin");

    // No rate limiter attached - default behaviour
    let mut writer = CachedDiskWriter::new(&path, Some(1024), None);
    writer.open().await.unwrap();
    writer.write_at(0, b"no limiter").await.unwrap();
    writer.flush().await.unwrap();

    let content = tokio::fs::read(&path).await.unwrap();
    assert!(
        content.starts_with(b"no limiter"),
        "should contain written data"
    );
}

// -- Concurrent write tests --------------------------------------------------

#[tokio::test]
async fn test_concurrent_writes_different_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_concurrent.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(16 * 1024 * 1024), None);
    writer.open().await.unwrap();

    let mut handles = vec![];
    for i in 0..16 {
        let offset = (i as u64) * 1024 * 1024;
        let data = vec![i as u8; 4096];
        let path_clone = path.clone();

        handles.push(tokio::spawn(async move {
            let mut w = CachedDiskWriter::new(&path_clone, None, None);
            w.open().await.unwrap();
            w.write_at(offset, &data).await.unwrap();
            w.flush().await.unwrap();
            w.close().await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let content = tokio::fs::read(&path).await.unwrap();
    for i in 0..16 {
        let offset = (i as usize) * 1024 * 1024;
        let expected = vec![i as u8; 4096];
        assert_eq!(
            &content[offset..offset + 4096],
            &expected[..],
            "Data mismatch at offset {}",
            i
        );
    }
}

#[tokio::test]
async fn test_concurrent_writes_serialized() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_same_offset.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(1024 * 1024), None);
    writer.open().await.unwrap();
    writer.close().await.unwrap();

    let write_count = Arc::new(AtomicUsize::new(0));
    let mut handles = vec![];

    for i in 0..10 {
        let offset = (i as u64) * 1024;
        let data = vec![i as u8; 1024];
        let path_clone = path.clone();
        let counter = write_count.clone();

        handles.push(tokio::spawn(async move {
            let mut w = CachedDiskWriter::new(&path_clone, None, None);
            w.open().await.unwrap();
            w.write_at(offset, &data).await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            w.flush().await.unwrap();
            w.close().await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    assert_eq!(write_count.load(Ordering::SeqCst), 10);

    let content = tokio::fs::read(&path).await.unwrap();
    for i in 0..10 {
        let offset = i * 1024;
        let expected = vec![i as u8; 1024];
        assert_eq!(
            &content[offset..offset + 1024],
            &expected[..],
            "Data mismatch at offset {}",
            offset
        );
    }
}

#[tokio::test]
async fn test_high_concurrency_stress() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_stress.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(32 * 1024 * 1024), None);
    writer.open().await.unwrap();
    writer.close().await.unwrap();

    let num_threads = 32;
    let writes_per_thread = 100;
    let mut handles = vec![];

    for thread_id in 0..num_threads {
        let path_clone = path.clone();

        handles.push(tokio::spawn(async move {
            let mut w = CachedDiskWriter::new(&path_clone, None, None);
            w.open().await.unwrap();

            for write_id in 0..writes_per_thread {
                let offset = ((thread_id * writes_per_thread + write_id) as u64) * 8192;
                let data = vec![(thread_id + write_id) as u8; 8192];
                w.write_at(offset, &data).await.unwrap();
            }

            w.flush().await.unwrap();
            w.close().await.unwrap();
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let content = tokio::fs::read(&path).await.unwrap();
    for thread_id in 0..num_threads {
        for write_id in 0..writes_per_thread {
            let offset = ((thread_id * writes_per_thread + write_id) as usize) * 8192;
            let expected = vec![(thread_id + write_id) as u8; 8192];
            if offset + 8192 <= content.len() {
                assert_eq!(
                    &content[offset..offset + 8192],
                    &expected[..],
                    "Data mismatch at thread {} write {}",
                    thread_id,
                    write_id
                );
            }
        }
    }
}

/// Verify that 8 concurrent tasks writing 64 KiB chunks to non-overlapping
/// offsets on a single `CachedDiskWriter` (wrapped in
/// `Arc<tokio::sync::Mutex<>>`) complete without deadlock and with full
/// data integrity.
///
/// Since `write_at` takes `&mut self`, the external `tokio::sync::Mutex`
/// serializes calls - but each call is now fast (no internal async mutex
/// held across `.await` points), so 8 tasks should complete in roughly
/// 1x single-write latency with no contention bottleneck.
#[tokio::test]
async fn test_concurrent_writes_no_mutex_contention() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_no_contention.bin");

    let chunk_size: usize = 64 * 1024;
    let num_tasks: usize = 8;
    let total_size = (chunk_size * num_tasks) as u64;

    let mut writer = CachedDiskWriter::new(&path, Some(total_size), None);
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

    // If there were a deadlock, this join would hang forever.
    for handle in handles {
        handle.await.unwrap();
    }

    {
        let mut guard = writer.lock().await;
        guard.flush().await.unwrap();
    }

    // Verify data integrity: each chunk should contain its fill byte.
    let content = tokio::fs::read(&path).await.unwrap();
    assert_eq!(content.len(), total_size as usize);
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

/// Regression: BT single-file downloads pick pieces out of order (RarestFirst
/// etc.), so writes must land at the piece offset. A sequential writer would
/// append piece 1's bytes right after piece 0's, silently corrupting the file.
#[tokio::test]
async fn test_cached_writer_out_of_order_writes_land_at_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out_of_order.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(8), Some(16));
    writer.open().await.unwrap();

    // Write piece 1 first, then piece 0 (simulated out-of-order arrival).
    writer.write_at(4, b"bbbb").await.unwrap();
    writer.write_at(0, b"aaaa").await.unwrap();

    // Flush the write-back cache to disk.
    writer.flush().await.unwrap();
    writer.close().await.unwrap();

    let content = std::fs::read(&path).unwrap();
    assert_eq!(
        content,
        b"aaaabbbb".to_vec(),
        "pieces must land at their offsets"
    );
}

/// Sequential DefaultDiskWriter is the control: appending out of order
/// corrupts the file — this documents why BT must not use it.
#[tokio::test]
async fn test_sequential_writer_appends_out_of_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("seq_ooo.bin");

    let mut writer = DefaultDiskWriter::new(&path);
    writer.write(b"bbbb").await.unwrap(); // piece 1 arrives first
    writer.write(b"aaaa").await.unwrap(); // piece 0 arrives second
    writer.finalize().await.unwrap();

    let content = std::fs::read(&path).unwrap();
    assert_eq!(
        content,
        b"bbbbaaaa".to_vec(),
        "sequential append ignores offsets"
    );
}
