//! Benchmark: Lock-free AtomicU64 TokenBucket vs Mutex<TokenBucket> for
//! concurrent token acquisition.
//!
//! # What this measures
//!
//! 4 concurrent tasks each call `try_acquire(64 KiB)` in a tight loop. We
//! compare:
//!
//! - **Lock-free `RateLimiter`** (new): Hot path is pure atomic CAS — no mutex.
//!   `try_acquire_download` does a lazy refill (CAS on `last_refill_elapsed_ns`)
//!   then a CAS loop on `tokens_milli`. Multiple tasks proceed concurrently.
//!
//! - **`Mutex<TokenBucket>`** (old mock): All token accounting (refill + check
//!   + deduct) happens under a `std::sync::Mutex`. Every acquire serializes on
//!     the lock, even when tokens are plentiful.
//!
//! The rate is set to 1 GiB/s with a 1 GiB burst so all acquires succeed —
//! this isolates the contention overhead from rate-limiting sleep behavior.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use std::sync::Arc;
use std::time::Instant;

use aria2_core::rate_limiter::{RateLimiter, RateLimiterConfig};

// =========================================================================
// Old-style mutex-based token bucket (mock for comparison)
// =========================================================================

/// Inner state protected by the mutex.
struct MutexTokenBucketInner {
    /// Current available tokens (whole tokens, no milli-precision).
    tokens: u64,
    /// Maximum capacity (burst size).
    capacity: u64,
    /// Refill rate in tokens (bytes) per second.
    rate_per_sec: u64,
    /// Last refill timestamp.
    last_refill: Instant,
}

/// Old-style token bucket: all token accounting (refill + acquire) happens
/// under a `std::sync::Mutex`. Every concurrent acquire serializes on the lock.
///
/// This represents the pre-atomic implementation pattern. We use
/// `std::sync::Mutex` (not `tokio::sync::Mutex`) because the critical section
/// contains no `.await` points — `std::sync::Mutex` is actually faster for
/// short critical sections, giving the mutex approach its best chance.
struct MutexTokenBucket {
    inner: std::sync::Mutex<MutexTokenBucketInner>,
}

impl MutexTokenBucket {
    fn new(rate_per_sec: u64, burst: u64) -> Self {
        Self {
            inner: std::sync::Mutex::new(MutexTokenBucketInner {
                tokens: burst,
                capacity: burst,
                rate_per_sec,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Non-blocking try_acquire under the mutex.
    ///
    /// Acquires the lock, performs lazy refill, checks/deducts tokens, releases
    /// the lock. The lock serializes all concurrent callers.
    fn try_acquire(&self, bytes: u64) -> bool {
        let mut inner = self.inner.lock().unwrap();
        // Lazy refill based on elapsed wall-clock time.
        let now = Instant::now();
        let elapsed = now.duration_since(inner.last_refill);
        let refill = (elapsed.as_secs_f64() * inner.rate_per_sec as f64) as u64;
        if refill > 0 {
            inner.tokens = inner.tokens.saturating_add(refill).min(inner.capacity);
            inner.last_refill = now;
        }
        if inner.tokens >= bytes {
            inner.tokens -= bytes;
            true
        } else {
            false
        }
    }
}

// =========================================================================
// Benchmark
// =========================================================================

fn bench_lock_free_rate_limiter(c: &mut Criterion) {
    let chunk_size: u64 = 64 * 1024; // 64 KiB per acquire
    let num_tasks: usize = 4;
    let iterations: usize = 1_000; // per task
    let rate: u64 = 1024 * 1024 * 1024; // 1 GiB/s
    let burst: u64 = 1024 * 1024 * 1024; // 1 GiB burst (all acquires succeed)
    let total_bytes: u64 = chunk_size * (iterations * num_tasks) as u64;

    let rt = tokio::runtime::Runtime::new().unwrap();

    let mut group = c.benchmark_group("rate_limiter");
    group.throughput(Throughput::Bytes(total_bytes));

    // ── Lock-free AtomicU64 TokenBucket ──
    //
    // try_acquire_download is async (returns bool) but does not actually await
    // — it delegates to the synchronous atomic-CAS try_acquire on the inner
    // TokenBucket. Concurrent tasks proceed without mutex serialization.
    group.bench_function("LockFree_AtomicU64_4tasks", |b| {
        b.iter(|| {
            let cfg = RateLimiterConfig::new(Some(rate), None).with_burst(Some(burst), None);
            let rl = Arc::new(RateLimiter::new(&cfg));
            rt.block_on(async {
                let mut handles = Vec::with_capacity(num_tasks);
                for _ in 0..num_tasks {
                    let rl = rl.clone();
                    handles.push(tokio::spawn(async move {
                        for _ in 0..iterations {
                            rl.try_acquire_download(chunk_size).await;
                        }
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        });
    });

    // ── Mutex<TokenBucket> (old pattern) ──
    //
    // Every try_acquire call acquires the std::sync::Mutex. With 4 concurrent
    // tasks, all acquires serialize on the lock — even though the critical
    // section is tiny, the lock acquire/release overhead and serialization
    // dominate under contention.
    group.bench_function("Mutex_TokenBucket_4tasks", |b| {
        b.iter(|| {
            let bucket = Arc::new(MutexTokenBucket::new(rate, burst));
            rt.block_on(async {
                let mut handles = Vec::with_capacity(num_tasks);
                for _ in 0..num_tasks {
                    let bucket = bucket.clone();
                    handles.push(tokio::spawn(async move {
                        for _ in 0..iterations {
                            bucket.try_acquire(chunk_size);
                        }
                    }));
                }
                for h in handles {
                    h.await.unwrap();
                }
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_lock_free_rate_limiter);
criterion_main!(benches);
