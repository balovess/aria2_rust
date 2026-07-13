//! E2E throughput regression test (Phase F5).
//!
//! Downloads a 10 MiB file via 16 concurrent positioned writes and measures
//! throughput. The spec targets >= 1.5x improvement vs. the old
//! `Arc<Mutex<DirectDiskAdaptor>>` path which held an async lock across every
//! `.await` point and serialized all writes.
//!
//! # Spec vs. CI sizing
//!
//! The spec mentions 1 GiB, but we use 10 MiB for CI speed. The relative
//! improvement (new vs. old) is what matters, not the absolute size. With 16
//! non-overlapping segments each writing 640 KiB via OS-native `pwrite` /
//! `seek_write`, the new path should saturate disk throughput while the old
//! path would be bottlenecked on the single async mutex.
//!
//! # What is measured
//!
//! The timed region covers the 16 concurrent `open + write_bytes_at + flush`
//! operations. `flush` calls `sync_all` (fsync), so the measurement includes
//! durability overhead — this is intentional so the regression test guards
//! against both throughput and excessive fsync regressions.

use std::time::Instant;

use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
use aria2_core::filesystem::positioned_disk_writer::PositionedDiskWriter;
use bytes::Bytes;
use tempfile::TempDir;

/// Throughput regression: 16 concurrent positioned writes of 640 KiB each
/// (10 MiB total) must sustain at least `MIN_THROUGHPUT_MIB_S`.
///
/// The threshold is deliberately conservative to avoid flaky CI on slower
/// disks / VMs. Positioned `pwrite`/`seek_write` should comfortably exceed it
/// on any modern SSD; the test exists to catch regressions that re-introduce
/// global-lock serialization, not to benchmark peak hardware speed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_positioned_write_throughput_10mb_16segments() {
    let total_size: usize = 10 * 1024 * 1024; // 10 MiB
    let num_segments: usize = 16;
    // 10 MiB / 16 = 640 KiB per segment (divides evenly).
    let segment_size: usize = total_size / num_segments;
    assert_eq!(
        segment_size * num_segments,
        total_size,
        "total_size must divide evenly by num_segments"
    );

    let dir = TempDir::new().expect("failed to create temp dir");
    let path = dir.path().join("throughput_test.bin");

    // Pre-allocate the file with one writer so the 16 concurrent writers only
    // perform writes (no file-extension races). The writer is dropped (closing
    // its handle) before the concurrent writers open the file.
    {
        let mut w = PositionedDiskWriter::new(&path, Some(total_size as u64));
        w.open().await.expect("pre-allocate open failed");
        w.flush().await.expect("pre-allocate flush failed");
        // `w` dropped here -> file handle closed.
    }

    // 16 concurrent writes via SEPARATE writers (true OS-level concurrency).
    // Each task opens its own file descriptor to the same path and writes a
    // non-overlapping segment. `pwrite`/`seek_write` is atomic and
    // offset-based, so concurrent non-overlapping writes are safe.
    let start = Instant::now();
    let mut handles = Vec::with_capacity(num_segments);

    for i in 0..num_segments {
        let offset = (i * segment_size) as u64;
        // Distinct fill byte per segment for integrity verification.
        let data = Bytes::from(vec![(i as u8).wrapping_add(1); segment_size]);
        let path_clone = path.clone();
        handles.push(tokio::spawn(async move {
            let mut w = PositionedDiskWriter::new(&path_clone, None);
            w.open().await.expect("concurrent open failed");
            w.write_bytes_at(offset, data)
                .await
                .expect("concurrent write_bytes_at failed");
            w.flush().await.expect("concurrent flush failed");
        }));
    }

    // If the positioned-write path regressed to a global async mutex held
    // across await points, these joins would still complete (no deadlock) but
    // throughput would collapse — caught by the assertion below.
    for handle in handles {
        handle.await.expect("spawned write task panicked");
    }

    let elapsed = start.elapsed();
    let throughput = (total_size as f64) / elapsed.as_secs_f64().max(f64::MIN_POSITIVE);

    // Verify data integrity: each segment must contain its distinct fill byte.
    // This proves the concurrent non-overlapping writes did not corrupt each
    // other (which a buggy shared-cursor seek+write path would do).
    let content = tokio::fs::read(&path).await.expect("failed to read back file");
    assert_eq!(content.len(), total_size, "file size mismatch after writes");
    for i in 0..num_segments {
        let seg_start = i * segment_size;
        let expected = (i as u8).wrapping_add(1);
        let segment = &content[seg_start..seg_start + segment_size];
        assert!(
            segment.iter().all(|&b| b == expected),
            "data integrity failed in segment {}: expected fill byte {:#04x}",
            i,
            expected
        );
    }

    // Throughput assertion. 50 MiB/s is conservative — positioned writes on a
    // pre-allocated file should be much faster even with per-segment fsync.
    // Windows CI runners have notoriously slow disk I/O, so we use a relaxed
    // threshold there.
    let min_throughput_mib_s: f64 = if cfg!(windows) { 3.0 } else { 50.0 };
    let min_throughput = min_throughput_mib_s * 1024.0 * 1024.0;
    let throughput_mib_s = throughput / (1024.0 * 1024.0);
    assert!(
        throughput >= min_throughput,
        "throughput {:.1} MiB/s below minimum {:.1} MiB/s ({} segments, {} MiB, {:?})",
        throughput_mib_s,
        min_throughput_mib_s,
        num_segments,
        total_size / (1024 * 1024),
        elapsed
    );

    eprintln!(
        "Throughput: {:.1} MiB/s ({:.1} MB/s) with {} segments, {} MiB total, {:?} elapsed",
        throughput_mib_s,
        throughput / 1_000_000.0,
        num_segments,
        total_size / (1024 * 1024),
        elapsed
    );
}
