//! Benchmark: io_uring positioned writes vs sync pwrite.
//!
//! Linux only. Run with:
//! ```sh
//! cargo bench --bench io_uring_positioned_write_bench --features io_uring
//! ```
//!
//! This benchmark compares two positioned-write strategies for 4 concurrent
//! 4 KiB writes to non-overlapping offsets on a pre-allocated file:
//!
//! - **io_uring**: `IoUringDiskWriter` backed by `tokio_uring::fs::File`,
//!   driven inside `tokio_uring::start`. Each task opens its own file
//!   descriptor and submits an independent SQE; the kernel completes them
//!   in parallel.
//! - **pwrite**: `PositionedDiskWriter` backed by `std::fs::File::write_at`
//!   (synchronous `pwrite(2)`), driven inside a multi-threaded tokio runtime.
//!   Each task opens its own file descriptor and performs a blocking `pwrite`.
//!
//! Both strategies use separate writer instances per task (each with its own
//! fd) to achieve true OS-level concurrency for non-overlapping writes.
//!
//! On non-Linux platforms or without the `io_uring` feature, this bench crate
//! compiles to a no-op binary (empty `main`) so that `cargo bench` / `cargo
//! test` do not break the build.

// NOTE: We intentionally do NOT use `#![cfg(...)]` at the crate level because
// that would strip the `main` function on non-Linux platforms, causing a
// linker error for the bench binary (which uses `harness = false`). Instead,
// all benchmark logic lives in a cfg-gated module and we provide a fallback
// `main` for the off-platform case.

#![cfg_attr(not(all(target_os = "linux", feature = "io_uring")), allow(dead_code))]

#[cfg(all(target_os = "linux", feature = "io_uring"))]
mod bench_impl {
    use std::sync::Arc;

    use criterion::{Criterion, criterion_group};
    use tempfile::TempDir;

    use aria2_core::filesystem::disk_writer::SeekableDiskWriter;
    use aria2_core::filesystem::positioned_disk_writer::{IoUringDiskWriter, PositionedDiskWriter};

    /// Chunk size per write: 4 KiB.
    const CHUNK_SIZE: usize = 4 * 1024;
    /// Number of concurrent write tasks.
    const NUM_TASKS: usize = 4;

    /// Benchmark 4 concurrent 4 KiB writes via io_uring vs pwrite.
    fn bench_concurrent_4k(c: &mut Criterion) {
        let dir = TempDir::new().expect("failed to create temp dir");
        let iouring_path = dir.path().join("bench_iouring.bin");
        let pwrite_path = dir.path().join("bench_pwrite.bin");
        let total = (CHUNK_SIZE * NUM_TASKS) as u64;

        // Pre-create and pre-allocate both files so the benchmark measures only
        // open + write + close, not file creation / extension.
        {
            for path in [&iouring_path, &pwrite_path] {
                let f = std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .open(path)
                    .expect("failed to pre-create bench file");
                f.set_len(total).expect("failed to pre-allocate bench file");
            }
        }

        let mut group = c.benchmark_group("positioned_write_4k_x4_concurrent");
        group.sample_size(50);

        // ── io_uring ──────────────────────────────────────────
        group.bench_function("io_uring", |b| {
            b.iter(|| {
                tokio_uring::start(async {
                    let path = Arc::new(iouring_path.clone());
                    let mut handles = Vec::with_capacity(NUM_TASKS);

                    for i in 0..NUM_TASKS {
                        let offset = (i as u64) * CHUNK_SIZE as u64;
                        let fill = (i as u8) + 1;
                        let path = Arc::clone(&path);
                        let data = vec![fill; CHUNK_SIZE];

                        handles.push(tokio_uring::spawn(async move {
                            let mut writer = IoUringDiskWriter::new(&path, None);
                            writer.open().await.expect("io_uring open failed");
                            writer
                                .write_at(offset, &data)
                                .await
                                .expect("io_uring write_at failed");
                            writer.flush().await.expect("io_uring flush failed");
                            writer.close().await.expect("io_uring close failed");
                        }));
                    }

                    for h in handles {
                        h.await.expect("io_uring task panicked");
                    }
                });
            });
        });

        // ── pwrite (synchronous positioned write via std::fs) ──
        //
        // Use a multi-threaded tokio runtime with enough workers for true
        // parallelism across the 4 tasks.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(NUM_TASKS)
            .enable_all()
            .build()
            .expect("failed to build tokio runtime");

        group.bench_function("pwrite", |b| {
            b.iter(|| {
                rt.block_on(async {
                    let path = Arc::new(pwrite_path.clone());
                    let mut handles = Vec::with_capacity(NUM_TASKS);

                    for i in 0..NUM_TASKS {
                        let offset = (i as u64) * CHUNK_SIZE as u64;
                        let fill = (i as u8) + 1;
                        let path = Arc::clone(&path);
                        let data = vec![fill; CHUNK_SIZE];

                        handles.push(tokio::spawn(async move {
                            let mut writer = PositionedDiskWriter::new(&path, None);
                            writer.open().await.expect("pwrite open failed");
                            writer
                                .write_at(offset, &data)
                                .await
                                .expect("pwrite write_at failed");
                            writer.flush().await.expect("pwrite flush failed");
                        }));
                    }

                    for h in handles {
                        h.await.expect("pwrite task panicked");
                    }
                });
            });
        });

        group.finish();
    }

    criterion_group!(benches, bench_concurrent_4k);
}

// On Linux + io_uring feature: criterion_main! generates `fn main()`.
#[cfg(all(target_os = "linux", feature = "io_uring"))]
criterion::criterion_main!(bench_impl::benches);

// On all other platforms / without the feature: no-op main so the bench
// binary compiles cleanly without any io_uring dependencies.
#[cfg(not(all(target_os = "linux", feature = "io_uring")))]
fn main() {
    // io_uring benchmark is only available on Linux with the `io_uring` feature.
}
