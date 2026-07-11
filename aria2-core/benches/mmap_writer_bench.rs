//! Benchmark: MmapDiskWriter vs PositionedDiskWriter for sequential writes.
//!
//! # What this measures
//!
//! Sequential writes of 10 MiB in 64 KiB chunks to a pre-allocated file,
//! comparing two I/O strategies:
//!
//! - **MmapDiskWriter**: Writes are direct `memcpy` into the memory-mapped
//!   region — no per-write syscall. The OS handles write-back asynchronously.
//!   Optimal for sequential writes where the entire file fits in the page cache.
//!
//! - **PositionedDiskWriter**: Each write is a `pwrite`/`seek_write` syscall.
//!   More syscall overhead per write, but no mmap setup cost and no virtual
//!   memory pressure for very large files.
//!
//! 10 MiB is used instead of 100 MiB to keep CI wall time reasonable while
//! still being large enough to show the per-write overhead difference.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tempfile::TempDir;

use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
use aria2_core::filesystem::mmap_disk_writer::MmapDiskWriter;
use aria2_core::filesystem::positioned_disk_writer::PositionedDiskWriter;

fn bench_mmap_vs_positioned_sequential(c: &mut Criterion) {
    let total_size: u64 = 10 * 1024 * 1024; // 10 MiB
    let chunk_size: usize = 64 * 1024; // 64 KiB
    let num_chunks: usize = (total_size as usize) / chunk_size;

    let dir = TempDir::new().unwrap();
    let path_mmap = dir.path().join("mmap.bin");
    let path_pos = dir.path().join("pos.bin");
    let rt = tokio::runtime::Runtime::new().unwrap();

    // Pre-generate chunk data so allocation cost is excluded from the timed
    // region. Each chunk has a distinct fill byte for data integrity.
    let chunks: Vec<Vec<u8>> = (0..num_chunks)
        .map(|i| vec![(i % 256) as u8; chunk_size])
        .collect();

    let mut group = c.benchmark_group("mmap_vs_positioned");
    group.throughput(Throughput::Bytes(total_size));
    // I/O benchmarks are slow — reduce sample count.
    group.sample_size(10);

    // ── MmapDiskWriter: direct memory copy into mmap region ──
    group.bench_function("MmapDiskWriter_sequential_10MB", |b| {
        b.iter(|| {
            let _ = std::fs::remove_file(&path_mmap);
            rt.block_on(async {
                let mut w = MmapDiskWriter::new(&path_mmap, Some(total_size));
                w.open().await.unwrap();
                for (i, chunk) in chunks.iter().enumerate() {
                    let offset = (i as u64) * chunk_size as u64;
                    w.write_at(offset, chunk).await.unwrap();
                }
                w.flush().await.unwrap();
            });
        });
    });

    // ── PositionedDiskWriter: pwrite/seek_write per chunk ──
    group.bench_function("PositionedDiskWriter_sequential_10MB", |b| {
        b.iter(|| {
            let _ = std::fs::remove_file(&path_pos);
            rt.block_on(async {
                let mut w = PositionedDiskWriter::new(&path_pos, Some(total_size));
                w.open().await.unwrap();
                for (i, chunk) in chunks.iter().enumerate() {
                    let offset = (i as u64) * chunk_size as u64;
                    w.write_at(offset, chunk).await.unwrap();
                }
                w.flush().await.unwrap();
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_mmap_vs_positioned_sequential);
criterion_main!(benches);
