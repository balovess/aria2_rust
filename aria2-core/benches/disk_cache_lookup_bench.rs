//! Benchmark: WrDiskCache BTreeMap lookup vs VecDeque linear scan.
//!
//! # What this measures
//!
//! Random `read()` lookups against a cache populated with 10,000 entries,
//! comparing two data structures:
//!
//! - **WrDiskCache** (new): Backed by `BTreeMap<u64, CacheEntry>`. Each lookup
//!   uses `range(..=offset).next_back()` — O(log n) per lookup.
//!
//! - **LinearScanCache** (old mock): Backed by `VecDeque<LinearScanEntry>`.
//!   Each lookup scans from the front until a covering entry is found — O(n)
//!   per lookup on average.
//!
//! Both use `tokio::sync::Mutex` for the same locking semantics, so the only
//! variable is the data-structure lookup cost. 1,000 random lookups are
//! performed per iteration to keep wall time reasonable.

use criterion::{Criterion, Throughput, criterion_group, criterion_main};

use aria2_core::filesystem::disk_cache::WrDiskCache;

// =========================================================================
// Old-style linear-scan cache (mock for comparison)
// =========================================================================

/// A single cache entry in the linear-scan mock.
struct LinearScanEntry {
    offset: u64,
    data: bytes::Bytes,
}

/// Old-style cache backed by a `VecDeque` with linear-scan lookups.
///
/// Uses `tokio::sync::Mutex` (same as `WrDiskCache`) so the only difference
/// from `WrDiskCache` is the data structure: `VecDeque` (O(n) scan) vs
/// `BTreeMap` (O(log n) range query).
struct LinearScanCache {
    entries: tokio::sync::Mutex<std::collections::VecDeque<LinearScanEntry>>,
}

impl LinearScanCache {
    fn new() -> Self {
        Self {
            entries: tokio::sync::Mutex::new(std::collections::VecDeque::new()),
        }
    }

    /// Insert an entry at the back of the deque.
    async fn write(&self, offset: u64, data: bytes::Bytes) {
        self.entries
            .lock()
            .await
            .push_back(LinearScanEntry { offset, data });
    }

    /// Linear-scan read: iterate from the front until a covering entry is found.
    ///
    /// Returns `Some(slice)` if an entry fully covers `[offset, offset+length)`,
    /// or `None` if no covering entry exists. Worst case O(n), average O(n/2).
    async fn read(&self, offset: u64, length: u64) -> Option<bytes::Bytes> {
        let entries = self.entries.lock().await;
        let end = offset + length;
        for entry in entries.iter() {
            let entry_end = entry.offset + entry.data.len() as u64;
            if entry.offset <= offset && entry_end >= end {
                let start = (offset - entry.offset) as usize;
                let slice_end = start + length as usize;
                if slice_end <= entry.data.len() {
                    // Zero-copy slice (Bytes::slice is O(1) — refcount bump).
                    return Some(entry.data.slice(start..slice_end));
                }
            }
        }
        None
    }
}

// =========================================================================
// Benchmark
// =========================================================================

fn bench_disk_cache_lookup(c: &mut Criterion) {
    let num_entries: usize = 10_000;
    let entry_size: usize = 64; // 64 bytes per entry
    let num_lookups: usize = 1_000; // random lookups per iteration
    // 64 MB max — well above num_entries * entry_size (625 KB) so no eviction.
    let max_cache_bytes: usize = 64 * 1024 * 1024;

    let rt = tokio::runtime::Runtime::new().unwrap();

    // ── Populate BTreeMap cache (WrDiskCache) ──
    let btree_cache = rt.block_on(async {
        let cache = WrDiskCache::with_max_size_bytes(max_cache_bytes);
        for i in 0..num_entries as u64 {
            let offset = i * entry_size as u64;
            let data = bytes::Bytes::from(vec![i as u8; entry_size]);
            cache.write(offset, data).await.unwrap();
        }
        cache
    });

    // ── Populate VecDeque cache (LinearScanCache) ──
    let linear_cache = rt.block_on(async {
        let cache = LinearScanCache::new();
        for i in 0..num_entries as u64 {
            let offset = i * entry_size as u64;
            let data = bytes::Bytes::from(vec![i as u8; entry_size]);
            cache.write(offset, data).await;
        }
        cache
    });

    // ── Pre-generate shuffled lookup offsets (deterministic LCG shuffle) ──
    //
    // We shuffle the offsets so that lookups hit random positions rather than
    // sequential ones. For the VecDeque, sequential lookups would be unfairly
    // fast (front entries found in O(1)); random lookups give O(n/2) average.
    let mut lookup_offsets: Vec<u64> = (0..num_entries as u64)
        .map(|i| i * entry_size as u64)
        .collect();
    // Numerical Recipes LCG constants for a deterministic pseudo-random shuffle.
    let mut state: u64 = 42;
    for i in (1..lookup_offsets.len()).rev() {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (state >> 33) as usize % (i + 1);
        lookup_offsets.swap(i, j);
    }
    // Take only the first num_lookups offsets for each iteration.
    let lookup_offsets: Vec<u64> = lookup_offsets.into_iter().take(num_lookups).collect();

    let mut group = c.benchmark_group("disk_cache_lookup");
    group.throughput(Throughput::Elements(num_lookups as u64));

    // ── WrDiskCache BTreeMap lookup — O(log n) per lookup ──
    group.bench_function("WrDiskCache_BTreeMap_10k_entries", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut hits = 0u32;
                for &offset in &lookup_offsets {
                    if btree_cache
                        .read(offset, entry_size as u64)
                        .await
                        .unwrap()
                        .is_some()
                    {
                        hits += 1;
                    }
                }
                hits
            });
        });
    });

    // ── VecDeque linear scan — O(n) per lookup ──
    group.bench_function("VecDeque_LinearScan_10k_entries", |b| {
        b.iter(|| {
            rt.block_on(async {
                let mut hits = 0u32;
                for &offset in &lookup_offsets {
                    if linear_cache.read(offset, entry_size as u64).await.is_some() {
                        hits += 1;
                    }
                }
                hits
            });
        });
    });

    group.finish();
}

criterion_group!(benches, bench_disk_cache_lookup);
criterion_main!(benches);
