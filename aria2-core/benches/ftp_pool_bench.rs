//! Criterion benchmarks for FTP connection pool performance.
//!
//! Run with: cargo bench --bench ftp_pool_bench

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use std::time::Duration;

// Simulated connection times (based on real-world measurements)
const CONNECTION_ESTABLISH_TIME_MS: u64 = 10_000;
const CONNECTION_REUSE_TIME_MS: u64 = 1;

fn simulate_connection_establish() {
    std::thread::sleep(Duration::from_millis(CONNECTION_ESTABLISH_TIME_MS));
}

fn simulate_connection_reuse() {
    std::thread::sleep(Duration::from_millis(CONNECTION_REUSE_TIME_MS));
}

fn bench_without_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("connection_without_pool");

    for num_ops in [1, 5, 10, 20].iter() {
        group.throughput(Throughput::Elements(*num_ops as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_ops),
            num_ops,
            |b, &num_ops| {
                b.iter(|| {
                    for _ in 0..num_ops {
                        simulate_connection_establish();
                    }
                    black_box(())
                });
            },
        );
    }

    group.finish();
}

fn bench_with_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("connection_with_pool");

    for num_ops in [1, 5, 10, 20].iter() {
        group.throughput(Throughput::Elements(*num_ops as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(num_ops),
            num_ops,
            |b, &num_ops| {
                b.iter(|| {
                    // First: establish
                    simulate_connection_establish();
                    // Rest: reuse
                    for _ in 1..num_ops {
                        simulate_connection_reuse();
                    }
                    black_box(())
                });
            },
        );
    }

    group.finish();
}

fn bench_pool_comparison(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_comparison");

    let num_ops = 10;

    group.bench_function("without_pool_10_ops", |b| {
        b.iter(|| {
            for _ in 0..num_ops {
                simulate_connection_establish();
            }
            black_box(())
        });
    });

    group.bench_function("with_pool_10_ops", |b| {
        b.iter(|| {
            simulate_connection_establish();
            for _ in 1..num_ops {
                simulate_connection_reuse();
            }
            black_box(())
        });
    });

    group.finish();
}

fn bench_lru_eviction(c: &mut Criterion) {
    let mut group = c.benchmark_group("lru_eviction");

    // Simulate different cache hit rates
    for hit_rate in [0.25, 0.5, 0.75, 1.0].iter() {
        group.bench_with_input(
            BenchmarkId::new("hit_rate", format!("{:.0}%", hit_rate * 100.0)),
            hit_rate,
            |b, &hit_rate| {
                let num_ops = 20;
                let hits = (num_ops as f64 * hit_rate) as usize;
                let misses = num_ops - hits;

                b.iter(|| {
                    // Misses: establish new connections
                    for _ in 0..misses {
                        simulate_connection_establish();
                    }
                    // Hits: reuse connections
                    for _ in 0..hits {
                        simulate_connection_reuse();
                    }
                    black_box(())
                });
            },
        );
    }

    group.finish();
}

fn bench_concurrent_access(c: &mut Criterion) {
    let mut group = c.benchmark_group("concurrent_access");

    for num_threads in [2, 4, 8].iter() {
        group.bench_with_input(
            BenchmarkId::new("threads", num_threads),
            num_threads,
            |b, &num_threads| {
                b.iter(|| {
                    use std::thread;

                    let handles: Vec<_> = (0..num_threads)
                        .map(|_| {
                            thread::spawn(|| {
                                // Each thread: 1 establish + 4 reuses
                                simulate_connection_establish();
                                for _ in 0..4 {
                                    simulate_connection_reuse();
                                }
                            })
                        })
                        .collect();

                    for handle in handles {
                        handle.join().unwrap();
                    }
                    black_box(())
                });
            },
        );
    }

    group.finish();
}

fn bench_pool_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("pool_overhead");

    // Measure the overhead of pool operations
    group.bench_function("connection_establish", |b| {
        b.iter(|| {
            simulate_connection_establish();
            black_box(())
        });
    });

    group.bench_function("connection_reuse", |b| {
        b.iter(|| {
            simulate_connection_reuse();
            black_box(())
        });
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .measurement_time(Duration::from_secs(10))
        .sample_size(10);
    targets =
        bench_without_pool,
        bench_with_pool,
        bench_pool_comparison,
        bench_lru_eviction,
        bench_concurrent_access,
        bench_pool_overhead
}

criterion_main!(benches);
