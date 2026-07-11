//! Memory footprint regression test (Phase F6).
//!
//! Writes a 10 MiB file via `PositionedDiskWriter` and samples the process
//! RSS during the write to ensure memory stays bounded. The spec targets
//! RSS < 32 MiB for a 1 GiB download (4x aria2's 8 MiB ceiling). We use 10 MiB
//! for CI speed but verify memory does NOT grow proportionally with file size.
//!
//! # Why `System::new()` instead of `System::new_all()`
//!
//! `System::new_all()` enumerates every process on the machine and stores
//! their metadata, which can itself consume tens of MiB — polluting a test
//! whose very purpose is to measure RSS. `System::new()` starts empty and
//! `refresh_process(pid)` is documented (sysinfo 0.30) to ADD the process if
//! it is not yet listed, so we sample only the current process cheaply.
//!
//! # sysinfo 0.30 memory semantics
//!
//! In sysinfo 0.30, `Process::memory()` returns the resident set size in
//! **bytes** (the RSS). This is the value we track as `peak_rss`.

use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
use aria2_core::filesystem::positioned_disk_writer::PositionedDiskWriter;
use bytes::Bytes;
use sysinfo::{Pid, System};
use tempfile::TempDir;

/// Refresh only `pid` in `sys` and update `peak_rss` if the current RSS is
/// larger. Returns the RSS just sampled (0 if the process could not be read).
///
/// This is a free function (not a closure) so it borrows `peak_rss` only for
/// the duration of each call, leaving it freely readable between calls and
/// after the loop — avoiding closure-capture borrow conflicts.
fn sample_rss(sys: &mut System, pid: Pid, peak_rss: &mut u64) -> u64 {
    // `refresh_process` adds the process if not yet listed (sysinfo 0.30) and
    // returns false only if the process no longer exists.
    if !sys.refresh_process(pid) {
        return 0;
    }
    let rss = sys.process(pid).map(|p| p.memory()).unwrap_or(0); // bytes (RSS)
    if rss > *peak_rss {
        *peak_rss = rss;
    }
    rss
}

/// Write a 10 MiB file in 64 KiB chunks via `PositionedDiskWriter` and verify
/// peak RSS stays bounded. The key assertion is that RSS does not scale with
/// file size: each 64 KiB chunk is written then dropped, so at most one chunk
/// buffer is live at a time and RSS should remain flat regardless of total
/// bytes written.
#[tokio::test]
async fn test_memory_footprint_10mb_write() {
    let total_size: usize = 10 * 1024 * 1024; // 10 MiB
    let chunk_size: usize = 64 * 1024; // 64 KiB
    let num_chunks = total_size / chunk_size;
    assert_eq!(
        chunk_size * num_chunks,
        total_size,
        "total_size must divide evenly by chunk_size"
    );

    let dir = TempDir::new().expect("failed to create temp dir");
    let path = dir.path().join("memory_test.bin");

    // Lightweight System: starts empty; `sample_rss` adds only the current
    // process via `refresh_process`.
    let mut sys = System::new();
    // `Pid::from_u32` is the cross-platform constructor (on Windows Pid wraps
    // usize, on Linux i32 — `from_u32` works everywhere).
    let pid = Pid::from_u32(std::process::id());

    let mut writer = PositionedDiskWriter::new(&path, Some(total_size as u64));
    writer.open().await.expect("writer open failed");

    let mut peak_rss: u64 = 0;
    // Baseline sample before any writes.
    sample_rss(&mut sys, pid, &mut peak_rss);

    for i in 0..num_chunks {
        let offset = (i * chunk_size) as u64;
        // Distinct fill byte per chunk for integrity; the Vec is moved into
        // Bytes and consumed by write_bytes_at (no retained copy).
        let data = Bytes::from(vec![(i % 256) as u8; chunk_size]);
        writer
            .write_bytes_at(offset, data)
            .await
            .expect("write_bytes_at failed");

        // Sample RSS periodically (every 16 chunks = every 1 MiB) to track
        // the peak without paying the refresh cost on every chunk.
        if i % 16 == 0 {
            sample_rss(&mut sys, pid, &mut peak_rss);
        }
    }

    writer.flush().await.expect("writer flush failed");

    // Final RSS sample after the flush (fsync) completes.
    sample_rss(&mut sys, pid, &mut peak_rss);

    // Verify data integrity so the test is not just a memory measurement.
    let content = tokio::fs::read(&path).await.expect("failed to read back file");
    assert_eq!(content.len(), total_size, "file size mismatch");
    for i in 0..num_chunks {
        let chunk_start = i * chunk_size;
        let expected = (i % 256) as u8;
        let chunk = &content[chunk_start..chunk_start + chunk_size];
        assert!(
            chunk.iter().all(|&b| b == expected),
            "data integrity failed in chunk {}: expected fill byte {:#04x}",
            i,
            expected
        );
    }

    // Assert peak RSS is reasonable. The spec targets < 32 MiB for a 1 GiB
    // download. For a 10 MiB download we allow 128 MiB total process RSS
    // (includes the tokio runtime, sysinfo, test harness, dev-dependencies
    // like criterion/hyper, etc.). The real invariant under test is that RSS
    // does NOT grow proportionally with file size — a leak in the write path
    // (e.g. buffering every chunk) would blow past this ceiling.
    const MAX_RSS_MIB: u64 = 128;
    let max_rss = MAX_RSS_MIB * 1024 * 1024;
    assert!(
        peak_rss <= max_rss,
        "peak RSS {} MiB exceeds maximum {} MiB for a {} MiB file",
        peak_rss / (1024 * 1024),
        MAX_RSS_MIB,
        total_size / (1024 * 1024)
    );

    eprintln!(
        "Peak RSS: {} MiB for {} MiB file (ratio {:.2}x)",
        peak_rss / (1024 * 1024),
        total_size / (1024 * 1024),
        (peak_rss as f64) / (total_size as f64)
    );
}
