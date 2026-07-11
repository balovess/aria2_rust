//! Benchmark: PositionedDiskWriter vs old Arc<tokio::sync::Mutex<DirectDiskAdaptor>>
//! for concurrent non-overlapping writes.
//!
//! # What this measures
//!
//! Two strategies for 4 concurrent 1 MiB writes to non-overlapping offsets in
//! the same file:
//!
//! - **PositionedDiskWriter** (new): Each task opens its own writer (own file
//!   descriptor) and calls `pwrite`/`seek_write` at its offset. No shared mutex
//!   — writes proceed concurrently at the OS level because `pwrite` is atomic
//!   and does not mutate the shared file cursor.
//!
//! - **DirectDiskAdaptor + Arc<tokio::sync::Mutex<>>** (old): All tasks share a
//!   single adaptor behind a `tokio::sync::Mutex`. The mutex is held across
//!   `.await` points (seek + write_all), serializing every write. This is the
//!   legacy contention bottleneck the positioned-writer design eliminates.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;
use tempfile::TempDir;

use aria2_core::filesystem::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
use aria2_core::filesystem::positioned_disk_writer::PositionedDiskWriter;

fn bench_concurrent_positioned_writes(c: &mut Criterion) {
    let chunk_size: usize = 1024 * 1024; // 1 MiB
    let num_tasks: usize = 4;
    let total: u64 = (chunk_size * num_tasks) as u64;

    let dir = TempDir::new().unwrap();
    let path_pos = dir.path().join("pos.bin");
    let path_old = dir.path().join("old.bin");
    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("positioned_write");
    group.throughput(Throughput::Bytes(total));
    // I/O benchmarks are slow — reduce sample count to keep wall time reasonable.
    group.sample_size(10);

    // ── New: PositionedDiskWriter, 4 separate writers, true OS concurrency ──
    //
    // Each task opens its own file descriptor and uses pwrite/seek_write at a
    // non-overlapping offset. No shared lock — the OS schedules the writes
    // concurrently.
    group.bench_function("PositionedDiskWriter_concurrent_4x1MB", |b| {
        b.iter(|| {
            let _ = std::fs::remove_file(&path_pos);
            rt.block_on(async {
                // Pre-allocate the file via one transient writer so that the
                // concurrent writers can issue pwrite at arbitrary offsets
                // without per-write file extension.
                {
                    let mut w = PositionedDiskWriter::new(&path_pos, Some(total));
                    w.open().await.unwrap();
                    w.flush().await.unwrap();
                }

                let mut handles = Vec::with_capacity(num_tasks);
                for i in 0..num_tasks {
                    let path = path_pos.clone();
                    handles.push(tokio::spawn(async move {
                        let mut w = PositionedDiskWriter::new(&path, None);
                        w.open().await.unwrap();
                        let offset = (i as u64) * chunk_size as u64;
                        let data = vec![(i as u8) + 1; chunk_size];
                        w.write_at(offset, &data).await.unwrap();
                        w.flush().await.unwrap();
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        });
    });

    // ── Old: Arc<tokio::sync::Mutex<DirectDiskAdaptor>>, serialized writes ──
    //
    // All 4 tasks share one DirectDiskAdaptor behind a tokio mutex. Each write
    // (seek + write_all) holds the lock across .await points, serializing all
    // writes even though they target non-overlapping offsets.
    group.bench_function("OldMutexDirectDiskAdaptor_4x1MB", |b| {
        b.iter(|| {
            let _ = std::fs::remove_file(&path_old);
            rt.block_on(async {
                let adaptor = Arc::new(tokio::sync::Mutex::new(DirectDiskAdaptor::new()));
                {
                    let mut a = adaptor.lock().await;
                    a.open(&path_old).await.unwrap();
                    a.truncate(total).await.unwrap();
                }

                let mut handles = Vec::with_capacity(num_tasks);
                for i in 0..num_tasks {
                    let offset = (i as u64) * chunk_size as u64;
                    let data = vec![(i as u8) + 1; chunk_size];
                    let a = adaptor.clone();
                    handles.push(tokio::spawn(async move {
                        let mut guard = a.lock().await;
                        guard.write(offset, &data).await.unwrap();
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
                adaptor.lock().await.flush().await.unwrap();
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_concurrent_positioned_writes);
criterion_main!(benches);
