//! Benchmark: PositionedDiskWriter vs old Arc<tokio::sync::Mutex<DirectDiskAdaptor>>
//! for concurrent non-overlapping writes.
//!
//! # What this measures
//!
//! Two strategies for 4 concurrent 1 MiB writes to non-overlapping offsets in
//! the same file. File handles are opened and pre-sized once before the
//! measured iterations so both cases measure the write lifecycle rather than
//! setup and file-open costs.
//!
//! - **PositionedDiskWriter** (new): Each task owns a persistent writer (own
//!   file descriptor) and calls `pwrite`/`seek_write` at its offset. No shared
//!   mutex — writes proceed concurrently at the OS level because `pwrite` is
//!   atomic and does not mutate the shared file cursor.
//!
//! - **DirectDiskAdaptor + Arc<tokio::sync::Mutex<>>** (old): All tasks share
//!   one persistent adaptor behind a `tokio::sync::Mutex`. The mutex is held
//!   across `.await` points (seek + write_all), serializing every write. This
//!   is the legacy contention bottleneck the positioned-writer design
//!   eliminates.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;
use tempfile::TempDir;

use aria2_core::filesystem::disk_adaptor::{DirectDiskAdaptor, DiskAdaptor};
use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
use aria2_core::filesystem::positioned_disk_writer::PositionedDiskWriter;

async fn write_positioned(
    writer: &mut PositionedDiskWriter,
    offset: u64,
    value: u8,
    chunk_size: usize,
) {
    let data = vec![value; chunk_size];
    writer.write_at(offset, &data).await.unwrap();
    writer.flush().await.unwrap();
}

async fn write_positioned_batch(writers: &mut [PositionedDiskWriter; 4], chunk_size: usize) {
    let (first, rest) = writers.split_at_mut(1);
    let (second, rest) = rest.split_at_mut(1);
    let (third, fourth) = rest.split_at_mut(1);
    tokio::join!(
        write_positioned(&mut first[0], 0, 1, chunk_size),
        write_positioned(&mut second[0], chunk_size as u64, 2, chunk_size),
        write_positioned(&mut third[0], (chunk_size * 2) as u64, 3, chunk_size),
        write_positioned(&mut fourth[0], (chunk_size * 3) as u64, 4, chunk_size)
    );
}

fn bench_concurrent_positioned_writes(c: &mut Criterion) {
    let chunk_size: usize = 1024 * 1024; // 1 MiB
    let num_tasks: usize = 4;
    let total: u64 = (chunk_size * num_tasks) as u64;

    let dir = TempDir::new().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let (mut positioned_writers, old_adaptor) = rt.block_on(async {
        let path_pos = dir.path().join("pos.bin");
        let path_old = dir.path().join("old.bin");

        let mut positioned_writers = Vec::with_capacity(num_tasks);
        for _ in 0..num_tasks {
            let mut writer = PositionedDiskWriter::new(&path_pos, Some(total));
            writer.open().await.unwrap();
            positioned_writers.push(writer);
        }

        let old_adaptor = Arc::new(tokio::sync::Mutex::new(DirectDiskAdaptor::new()));
        {
            let mut adaptor = old_adaptor.lock().await;
            adaptor.open(&path_old).await.unwrap();
            adaptor.truncate(total).await.unwrap();
        }

        let positioned_writers = match positioned_writers.try_into() {
            Ok(writers) => writers,
            Err(_) => unreachable!("benchmark uses exactly four positioned writers"),
        };
        (positioned_writers, old_adaptor)
    });

    let mut group = c.benchmark_group("positioned_write");
    group.throughput(Throughput::Bytes(total));
    // I/O benchmarks are slow — reduce sample count to keep wall time reasonable.
    group.sample_size(10);

    // ── New: PositionedDiskWriter, 4 persistent writers, true OS concurrency ──
    //
    // Each task uses its own persistent file descriptor and calls
    // pwrite/seek_write at a non-overlapping offset. No shared lock — the OS
    // schedules the writes concurrently.
    group.bench_function("PositionedDiskWriter_concurrent_4x1MB", |b| {
        b.iter(|| {
            rt.block_on(write_positioned_batch(&mut positioned_writers, chunk_size));
        });
    });

    // ── Old: persistent Arc<tokio::sync::Mutex<DirectDiskAdaptor>> ──
    //
    // All 4 tasks share one DirectDiskAdaptor behind a tokio mutex. Each write
    // (seek + write_all) holds the lock across .await points, serializing all
    // writes even though they target non-overlapping offsets.
    group.bench_function("OldMutexDirectDiskAdaptor_4x1MB", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut handles = Vec::with_capacity(num_tasks);
                for i in 0..num_tasks {
                    let offset = (i as u64) * chunk_size as u64;
                    let data = vec![(i as u8) + 1; chunk_size];
                    let adaptor = old_adaptor.clone();
                    handles.push(tokio::spawn(async move {
                        let mut guard = adaptor.lock().await;
                        guard.write(offset, &data).await.unwrap();
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
                old_adaptor.lock().await.flush().await.unwrap();
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_concurrent_positioned_writes);
criterion_main!(benches);
