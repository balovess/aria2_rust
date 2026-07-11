//! Disk I/O Performance Analysis Tests
//!
//! This module provides comprehensive performance analysis for disk I/O operations:
//! - Cache hit rate measurement for CachedDiskWriter
//! - Lock contention analysis for concurrent writes
//! - File preallocation strategy impact
//! - fsync frequency impact on performance and data safety
//! - Throughput comparison of different write strategies

use aria2_core::filesystem::disk_cache::WrDiskCache;
use aria2_core::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use aria2_core::filesystem::file_allocation::preallocate_file;
use aria2_core::util::perf_monitor::{AtomicMetrics, Metrics, PerformanceMonitor};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

// =============================================================================
// 5.1 Cache Hit Rate Measurement
// =============================================================================

/// Statistics for cache performance measurement
#[derive(Debug, Default)]
struct CacheStats {
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    total_reads: AtomicU64,
    total_writes: AtomicU64,
}

impl CacheStats {
    fn new() -> Self {
        Self::default()
    }

    fn record_hit(&self) {
        self.cache_hits.fetch_add(1, Ordering::Relaxed);
        self.total_reads.fetch_add(1, Ordering::Relaxed);
    }

    fn record_miss(&self) {
        self.cache_misses.fetch_add(1, Ordering::Relaxed);
        self.total_reads.fetch_add(1, Ordering::Relaxed);
    }

    fn record_write(&self) {
        self.total_writes.fetch_add(1, Ordering::Relaxed);
    }

    fn hit_rate(&self) -> f64 {
        let hits = self.cache_hits.load(Ordering::Relaxed);
        let total = self.total_reads.load(Ordering::Relaxed);
        if total == 0 {
            0.0
        } else {
            hits as f64 / total as f64
        }
    }

    fn snapshot(&self) -> (u64, u64, u64, u64) {
        (
            self.cache_hits.load(Ordering::Relaxed),
            self.cache_misses.load(Ordering::Relaxed),
            self.total_reads.load(Ordering::Relaxed),
            self.total_writes.load(Ordering::Relaxed),
        )
    }
}

/// Test cache hit rate with sequential writes and reads
#[tokio::test]
async fn test_cache_hit_rate_sequential() {
    let dir = tempfile::tempdir().unwrap();
    let _path = dir.path().join("cache_seq.bin");
    let stats = Arc::new(CacheStats::new());

    // Create cache with 4MB capacity
    let cache = Arc::new(WrDiskCache::new(4));

    // Write 100 small blocks (4KB each)
    let block_size = 4 * 1024;
    let num_blocks = 100;

    let start = Instant::now();
    for i in 0..num_blocks {
        let offset = (i * block_size) as u64;
        let data: bytes::Bytes = vec![i as u8; block_size].into();
        cache.write(offset, data).await.unwrap();
        stats.record_write();
    }
    let write_duration = start.elapsed();

    // Read back all blocks (should all be cache hits if not evicted)
    let start = Instant::now();
    for i in 0..num_blocks {
        let offset = (i * block_size) as u64;
        let result = cache.read(offset, block_size as u64).await.unwrap();
        if result.is_some() {
            stats.record_hit();
        } else {
            stats.record_miss();
        }
    }
    let read_duration = start.elapsed();

    let (hits, misses, total_reads, total_writes) = stats.snapshot();
    let hit_rate = stats.hit_rate();

    println!("\n=== Cache Hit Rate Test (Sequential) ===");
    println!("Cache size: 4 MB");
    println!("Block size: {} bytes", block_size);
    println!("Total writes: {}", total_writes);
    println!("Total reads: {}", total_reads);
    println!("Cache hits: {}", hits);
    println!("Cache misses: {}", misses);
    println!("Hit rate: {:.2}%", hit_rate * 100.0);
    println!("Write duration: {:?}", write_duration);
    println!("Read duration: {:?}", read_duration);
    println!("Write throughput: {:.2} MB/s",
        (total_writes * block_size as u64) as f64 / write_duration.as_secs_f64() / 1_000_000.0);

    // With 4MB cache and 400KB total data, all reads should hit cache
    assert!(hit_rate > 0.8, "Expected high hit rate, got {:.2}%", hit_rate * 100.0);
}

/// Test cache hit rate with random access pattern
#[tokio::test]
async fn test_cache_hit_rate_random_access() {
    let _dir = tempfile::tempdir().unwrap();
    let stats = Arc::new(CacheStats::new());

    // Create cache with 1MB capacity
    let cache = Arc::new(WrDiskCache::new(1));

    // Write 200 small blocks (8KB each = 1.6MB total, will cause eviction)
    let block_size = 8 * 1024;
    let num_blocks = 200;

    let start = Instant::now();
    for i in 0..num_blocks {
        let offset = (i * block_size) as u64;
        let data: bytes::Bytes = vec![((i * 7) % 256) as u8; block_size].into();
        cache.write(offset, data).await.unwrap();
        stats.record_write();
    }
    let write_duration = start.elapsed();

    // Flush to make entries clean (eligible for eviction)
    cache.flush().await.unwrap();

    // Write more to trigger eviction
    for i in num_blocks..(num_blocks + 50) {
        let offset = (i * block_size) as u64;
        let data: bytes::Bytes = vec![((i * 7) % 256) as u8; block_size].into();
        cache.write(offset, data).await.unwrap();
        stats.record_write();
    }

    // Try to read random blocks (some will have been evicted)
    let start = Instant::now();
    for i in (0..num_blocks).step_by(3) {
        let offset = (i * block_size) as u64;
        let result = cache.read(offset, block_size as u64).await.unwrap();
        if result.is_some() {
            stats.record_hit();
        } else {
            stats.record_miss();
        }
    }
    let read_duration = start.elapsed();

    let (hits, misses, total_reads, total_writes) = stats.snapshot();
    let hit_rate = stats.hit_rate();

    println!("\n=== Cache Hit Rate Test (Random Access) ===");
    println!("Cache size: 1 MB");
    println!("Block size: {} bytes", block_size);
    println!("Total writes: {}", total_writes);
    println!("Total reads: {}", total_reads);
    println!("Cache hits: {}", hits);
    println!("Cache misses: {}", misses);
    println!("Hit rate: {:.2}%", hit_rate * 100.0);
    println!("Write duration: {:?}", write_duration);
    println!("Read duration: {:?}", read_duration);

    // With eviction, hit rate should be lower than sequential
    println!("Expected: Lower hit rate due to LRU eviction");
}

/// Test cache eviction behavior with dirty entries
#[tokio::test]
async fn test_cache_eviction_dirty_safety() {
    let stats = Arc::new(CacheStats::new());

    // Create small cache (256KB) to force eviction
    let cache = Arc::new(WrDiskCache::with_max_size_bytes(256 * 1024));

    // Write 10 dirty blocks (32KB each = 320KB total, exceeds cache)
    let block_size = 32 * 1024;
    let num_blocks = 10;

    let start = Instant::now();
    for i in 0..num_blocks {
        let offset = (i * block_size) as u64;
        let data: bytes::Bytes = vec![i as u8; block_size].into();
        cache.write(offset, data).await.unwrap();
        stats.record_write();
    }
    let write_duration = start.elapsed();

    // All dirty entries should still be present (no eviction of dirty data)
    let count = cache.count().await;
    let dirty_count = cache.dirty_count().await;

    println!("\n=== Cache Eviction Dirty Safety Test ===");
    println!("Cache size: 256 KB");
    println!("Total data written: {} KB", (num_blocks * block_size) / 1024);
    println!("Entries in cache: {}", count);
    println!("Dirty entries: {}", dirty_count);
    println!("Write duration: {:?}", write_duration);

    // All entries should be present since they're all dirty
    assert_eq!(count, num_blocks, "All dirty entries should be preserved");
    assert_eq!(dirty_count, num_blocks, "All entries should be dirty");

    // Verify data integrity
    for i in 0..num_blocks {
        let offset = (i * block_size) as u64;
        let result = cache.read(offset, block_size as u64).await.unwrap();
        assert!(result.is_some(), "Dirty entry at offset {} should exist", offset);
        let data = result.unwrap();
        assert!(data.iter().all(|&b| b == i as u8), "Data integrity check failed");
    }
}

// =============================================================================
// 5.2 Lock Contention Analysis
// =============================================================================

/// Measure lock contention with single Mutex (current implementation)
#[tokio::test]
async fn test_lock_contention_single_mutex() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock_single.bin");

    let metrics = Arc::new(AtomicMetrics::new());
    let writer = Arc::new(Mutex::new(CachedDiskWriter::new(&path, Some(10 * 1024 * 1024), None)));

    // Open the writer first
    {
        let mut w = writer.lock().await;
        w.open().await.unwrap();
    }

    let num_tasks = 8;
    let writes_per_task = 100;
    let block_size = 4 * 1024;

    let start = Instant::now();
    let mut handles = vec![];

    for task_id in 0..num_tasks {
        let writer = writer.clone();
        let metrics = metrics.clone();

        handles.push(tokio::spawn(async move {
            for i in 0..writes_per_task {
                let lock_start = Instant::now();
                let mut w = writer.lock().await;
                let lock_wait = lock_start.elapsed();

                let offset = ((task_id * writes_per_task + i) * block_size) as u64;
                let data = vec![task_id as u8; block_size];
                w.write_at(offset, &data).await.unwrap();

                metrics.record_lock_wait(lock_wait.as_millis() as u64);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let total_duration = start.elapsed();
    let snapshot = metrics.snapshot();

    println!("\n=== Lock Contention Test (Single Mutex) ===");
    println!("Concurrent tasks: {}", num_tasks);
    println!("Writes per task: {}", writes_per_task);
    println!("Total writes: {}", num_tasks * writes_per_task);
    println!("Total duration: {:?}", total_duration);
    println!("Total lock wait time: {} ms", snapshot.lock_wait_time);
    println!("Average lock wait: {:.2} ms",
        snapshot.lock_wait_time as f64 / (num_tasks * writes_per_task) as f64);
    println!("Throughput: {:.2} MB/s",
        (num_tasks * writes_per_task * block_size) as f64 / total_duration.as_secs_f64() / 1_000_000.0);

    // Flush and verify
    {
        let mut w = writer.lock().await;
        w.flush().await.unwrap();
    }

    let file_size = tokio::fs::metadata(&path).await.unwrap().len();
    println!("Final file size: {} bytes", file_size);
}

/// Measure lock contention with striped locks (improved implementation)
#[tokio::test]
async fn test_lock_contention_striped_locks() {
    let dir = tempfile::tempdir().unwrap();
    let num_stripes = 4;
    let stripes: Vec<Arc<Mutex<CachedDiskWriter>>> = (0..num_stripes)
        .map(|i| {
            let path = dir.path().join(format!("stripe_{}.bin", i));
            Arc::new(Mutex::new(CachedDiskWriter::new(&path, Some(2 * 1024 * 1024), None)))
        })
        .collect();

    let metrics = Arc::new(AtomicMetrics::new());

    // Open all writers
    for stripe in &stripes {
        let mut w = stripe.lock().await;
        w.open().await.unwrap();
    }

    let num_tasks = 8;
    let writes_per_task = 100;
    let block_size = 4 * 1024;

    let start = Instant::now();
    let mut handles = vec![];

    for task_id in 0..num_tasks {
        let stripes = stripes.clone();
        let metrics = metrics.clone();

        handles.push(tokio::spawn(async move {
            for i in 0..writes_per_task {
                // Hash to determine which stripe to use
                let stripe_idx = (task_id * writes_per_task + i) % num_stripes;
                let writer = &stripes[stripe_idx];

                let lock_start = Instant::now();
                let mut w = writer.lock().await;
                let lock_wait = lock_start.elapsed();

                let offset = ((task_id * writes_per_task + i) * block_size) as u64;
                let data = vec![task_id as u8; block_size];
                w.write_at(offset, &data).await.unwrap();

                metrics.record_lock_wait(lock_wait.as_millis() as u64);
            }
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let total_duration = start.elapsed();
    let snapshot = metrics.snapshot();

    println!("\n=== Lock Contention Test (Striped Locks) ===");
    println!("Number of stripes: {}", num_stripes);
    println!("Concurrent tasks: {}", num_tasks);
    println!("Writes per task: {}", writes_per_task);
    println!("Total writes: {}", num_tasks * writes_per_task);
    println!("Total duration: {:?}", total_duration);
    println!("Total lock wait time: {} ms", snapshot.lock_wait_time);
    println!("Average lock wait: {:.2} ms",
        snapshot.lock_wait_time as f64 / (num_tasks * writes_per_task) as f64);
    println!("Throughput: {:.2} MB/s",
        (num_tasks * writes_per_task * block_size) as f64 / total_duration.as_secs_f64() / 1_000_000.0);

    // Flush all stripes
    for stripe in &stripes {
        let mut w = stripe.lock().await;
        w.flush().await.unwrap();
    }
}

// =============================================================================
// 5.3 File Preallocation Strategy Impact
// =============================================================================

/// Test file preallocation strategies performance
#[tokio::test]
async fn test_preallocation_strategy_performance() {
    let dir = tempfile::tempdir().unwrap();
    let file_size = 100 * 1024 * 1024; // 100 MB

    let strategies = ["none", "trunc", "prealloc", "falloc"];

    println!("\n=== File Preallocation Strategy Performance ===");
    println!("File size: {} MB", file_size / 1024 / 1024);

    for strategy in &strategies {
        let path = dir.path().join(format!("prealloc_{}.bin", strategy));

        let start = Instant::now();
        preallocate_file(&path, file_size, strategy, false).await.unwrap();
        let duration = start.elapsed();

        let metadata = tokio::fs::metadata(&path).await;
        let actual_size = if strategy == &"none" {
            0
        } else {
            metadata.unwrap().len()
        };

        println!("\nStrategy: {}", strategy);
        println!("  Allocation time: {:?}", duration);
        println!("  Actual size: {} bytes", actual_size);
        println!("  Throughput: {:.2} MB/s",
            actual_size as f64 / duration.as_secs_f64() / 1_000_000.0);

        // Clean up
        if *strategy != "none" {
            tokio::fs::remove_file(&path).await.ok();
        }
    }
}

/// Test write performance with preallocated vs non-preallocated files
#[tokio::test]
async fn test_write_performance_with_preallocation() {
    let dir = tempfile::tempdir().unwrap();
    let file_size: usize = 50 * 1024 * 1024; // 50 MB
    let block_size: usize = 64 * 1024; // 64 KB blocks
    let num_blocks = file_size / block_size;

    println!("\n=== Write Performance With/Without Preallocation ===");
    println!("File size: {} MB", file_size / 1024 / 1024);
    println!("Block size: {} KB", block_size / 1024);
    println!("Number of blocks: {}", num_blocks);

    // Test without preallocation
    {
        let path = dir.path().join("no_preallocation.bin");
        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();
        }
        writer.flush().await.unwrap();
        let duration = start.elapsed();

        println!("\nWithout preallocation:");
        println!("  Write time: {:?}", duration);
        println!("  Throughput: {:.2} MB/s",
            file_size as f64 / duration.as_secs_f64() / 1_000_000.0);
    }

    // Test with preallocation
    {
        let path = dir.path().join("with_preallocation.bin");
        preallocate_file(&path, file_size as u64, "falloc", false).await.unwrap();

        let mut writer = CachedDiskWriter::new(&path, Some(file_size as u64), None);
        writer.open().await.unwrap();

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();
        }
        writer.flush().await.unwrap();
        let duration = start.elapsed();

        println!("\nWith preallocation:");
        println!("  Write time: {:?}", duration);
        println!("  Throughput: {:.2} MB/s",
            file_size as f64 / duration.as_secs_f64() / 1_000_000.0);
    }
}

// =============================================================================
// 5.4 fsync Frequency Impact
// =============================================================================

/// Test fsync frequency impact on performance
#[tokio::test]
async fn test_fsync_frequency_impact() {
    let dir = tempfile::tempdir().unwrap();
    let file_size = 20 * 1024 * 1024; // 20 MB
    let block_size = 64 * 1024; // 64 KB
    let num_blocks = file_size / block_size;

    println!("\n=== fsync Frequency Impact ===");
    println!("File size: {} MB", file_size / 1024 / 1024);
    println!("Block size: {} KB", block_size / 1024);
    println!("Number of blocks: {}", num_blocks);

    // Test with different fsync frequencies
    let fsync_intervals = [1, 10, 50, 100, 1000, num_blocks]; // fsync every N blocks

    for interval in &fsync_intervals {
        let path = dir.path().join(format!("fsync_{}.bin", interval));
        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();

            // fsync at specified interval
            if (i + 1) % interval == 0 {
                writer.flush().await.unwrap();
            }
        }
        // Final flush
        writer.flush().await.unwrap();
        let duration = start.elapsed();

        let num_fsyncs = num_blocks / interval + 1;

        println!("\nfsync every {} blocks:", interval);
        println!("  Total fsyncs: {}", num_fsyncs);
        println!("  Write time: {:?}", duration);
        println!("  Throughput: {:.2} MB/s",
            file_size as f64 / duration.as_secs_f64() / 1_000_000.0);
        println!("  Time per fsync: {:.2} ms",
            duration.as_millis() as f64 / num_fsyncs as f64);

        // Clean up
        tokio::fs::remove_file(&path).await.ok();
    }
}

/// Test data safety with different fsync strategies
#[tokio::test]
async fn test_fsync_data_safety() {
    let dir = tempfile::tempdir().unwrap();

    println!("\n=== fsync Data Safety Test ===");

    // Simulate crash scenario: write data without fsync
    {
        let path = dir.path().join("no_fsync.bin");
        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        // Write data
        writer.write_at(0, b"important data").await.unwrap();
        // No flush/fsync - data may not be on disk

        // In a real crash scenario, this data could be lost
        println!("Without fsync: Data may be lost on crash (in OS cache)");
    }

    // With fsync - data is guaranteed on disk
    {
        let path = dir.path().join("with_fsync.bin");
        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        // Write data
        writer.write_at(0, b"important data").await.unwrap();
        // Explicit flush
        writer.flush().await.unwrap();

        println!("With fsync: Data is durable on disk");
    }

    // Trade-off analysis
    println!("\nTrade-off Analysis:");
    println!("  - No fsync: Fastest, but data loss risk on crash");
    println!("  - fsync every write: Safest, but slowest");
    println!("  - fsync every N writes: Balanced approach");
    println!("  - Recommended: fsync every 50-100 writes for downloads");
}

// =============================================================================
// 5.5 Write Strategy Throughput Comparison
// =============================================================================

/// Compare throughput of different write strategies
#[tokio::test]
async fn test_write_strategy_throughput_comparison() {
    let dir = tempfile::tempdir().unwrap();
    let total_size = 50 * 1024 * 1024; // 50 MB

    println!("\n=== Write Strategy Throughput Comparison ===");
    println!("Total size: {} MB", total_size / 1024 / 1024);

    // Strategy 1: Direct write (no cache)
    {
        let path = dir.path().join("direct_write.bin");
        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        let block_size = 64 * 1024; // 64 KB
        let num_blocks = total_size / block_size;

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();
        }
        writer.flush().await.unwrap();
        let duration = start.elapsed();

        println!("\n1. Direct write (no cache):");
        println!("   Block size: {} KB", block_size / 1024);
        println!("   Time: {:?}", duration);
        println!("   Throughput: {:.2} MB/s",
            total_size as f64 / duration.as_secs_f64() / 1_000_000.0);
    }

    // Strategy 2: Cached write (with cache)
    {
        let path = dir.path().join("cached_write.bin");
        let mut writer = CachedDiskWriter::new(&path, None, Some(4)); // 4 MB cache
        writer.open().await.unwrap();

        let block_size = 4 * 1024; // 4 KB (smaller blocks benefit from cache)
        let num_blocks = total_size / block_size;

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();
        }
        writer.flush().await.unwrap();
        let duration = start.elapsed();

        println!("\n2. Cached write (4 MB cache):");
        println!("   Block size: {} KB", block_size / 1024);
        println!("   Time: {:?}", duration);
        println!("   Throughput: {:.2} MB/s",
            total_size as f64 / duration.as_secs_f64() / 1_000_000.0);
    }

    // Strategy 3: Large block direct write
    {
        let path = dir.path().join("large_block_write.bin");
        let mut writer = CachedDiskWriter::new(&path, None, None);
        writer.open().await.unwrap();

        let block_size = 256 * 1024; // 256 KB
        let num_blocks = total_size / block_size;

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();
        }
        writer.flush().await.unwrap();
        let duration = start.elapsed();

        println!("\n3. Large block direct write:");
        println!("   Block size: {} KB", block_size / 1024);
        println!("   Time: {:?}", duration);
        println!("   Throughput: {:.2} MB/s",
            total_size as f64 / duration.as_secs_f64() / 1_000_000.0);
    }

    // Strategy 4: Batched writes
    {
        use aria2_core::engine::batched_disk_writer::BatchedDiskWriter;
        let path = dir.path().join("batched_write.bin");
        let mut writer = BatchedDiskWriter::new(&path)
            .with_threshold(256 * 1024); // 256 KB batch threshold

        let block_size = 16 * 1024; // 16 KB
        let num_blocks = total_size / block_size;

        let start = Instant::now();
        for i in 0..num_blocks {
            let offset = (i * block_size) as u64;
            let data = vec![i as u8; block_size];
            writer.write_at(offset, &data).await.unwrap();
        }
        writer.close().await.unwrap();
        let duration = start.elapsed();

        println!("\n4. Batched write (256 KB threshold):");
        println!("   Block size: {} KB", block_size / 1024);
        println!("   Time: {:?}", duration);
        println!("   Throughput: {:.2} MB/s",
            total_size as f64 / duration.as_secs_f64() / 1_000_000.0);
    }
}

/// Test fixed threshold behavior
#[tokio::test]
async fn test_fixed_threshold_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixed_threshold.bin");

    let mut writer = CachedDiskWriter::new(&path, Some(10 * 1024 * 1024), None);

    writer.open().await.unwrap();

    println!("\n=== Fixed Threshold Behavior ===");
    println!("Threshold: 1 MB (fixed)");

    // Write mix of small and large blocks
    let small_size = 4 * 1024; // 4 KB
    let large_size = 512 * 1024; // 512 KB

    for i in 0..200 {
        let (size, offset) = if i % 5 == 0 {
            // 20% large writes
            (large_size, i * large_size)
        } else {
            // 80% small writes
            (small_size, i * small_size)
        };

        let data = vec![i as u8; size];
        writer.write_at(offset as u64, &data).await.unwrap();
    }

    writer.flush().await.unwrap();
}

// =============================================================================
// 5.6 Comprehensive Performance Report
// =============================================================================

/// Generate comprehensive disk I/O performance report
#[tokio::test]
async fn test_generate_disk_io_performance_report() {
    let monitor = Arc::new(PerformanceMonitor::new());

    println!("\n{}", "=".repeat(80));
    println!("DISK I/O PERFORMANCE ANALYSIS REPORT");
    println!("Generated at: {:?}", std::time::SystemTime::now());
    println!("{}", "=".repeat(80));

    // Run all tests and collect metrics
    let dir = tempfile::tempdir().unwrap();

    // Test 1: Cache performance
    let cache_metrics = test_cache_performance(&dir).await;
    monitor.record_metric("cache_performance", cache_metrics);

    // Test 2: Lock contention
    let lock_metrics = test_lock_performance(&dir).await;
    monitor.record_metric("lock_contention", lock_metrics);

    // Test 3: Preallocation
    let prealloc_metrics = test_preallocation_performance(&dir).await;
    monitor.record_metric("preallocation", prealloc_metrics);

    // Test 4: fsync impact
    let fsync_metrics = test_fsync_performance(&dir).await;
    monitor.record_metric("fsync_impact", fsync_metrics);

    // Test 5: Write throughput
    let throughput_metrics = test_throughput_performance(&dir).await;
    monitor.record_metric("write_throughput", throughput_metrics);

    // Generate report
    let report = monitor.generate_report();
    println!("\n{}", monitor.export_text());

    // Summary
    println!("\n{}", "=".repeat(80));
    println!("SUMMARY");
    println!("{}", "=".repeat(80));
    println!("Total samples: {}", report.summary.total_samples);
    println!("Average throughput: {} bytes/sec", report.summary.avg_throughput);
    println!("Average latency: {} ms", report.summary.avg_latency);
    println!("Average memory usage: {} bytes", report.summary.avg_memory_usage);
    println!("Average lock wait time: {} ms", report.summary.avg_lock_wait_time);
    println!("{}", "=".repeat(80));
}

async fn test_cache_performance(_dir: &tempfile::TempDir) -> Metrics {
    let cache = WrDiskCache::new(4);
    let block_size = 4 * 1024;
    let num_blocks = 100;

    let start = Instant::now();
    for i in 0..num_blocks {
        let offset = (i * block_size) as u64;
        let data: bytes::Bytes = vec![i as u8; block_size].into();
        cache.write(offset, data).await.unwrap();
    }
    let duration = start.elapsed();

    let throughput = (num_blocks * block_size) as f64 / duration.as_secs_f64();
    Metrics::new(throughput as u64, duration.as_millis() as u64, cache.current_size_bytes() as u64, 0)
        .with_label("cache_write")
}

async fn test_lock_performance(dir: &tempfile::TempDir) -> Metrics {
    let path = dir.path().join("lock_test.bin");
    let writer = Arc::new(Mutex::new(CachedDiskWriter::new(&path, Some(1024 * 1024), None)));

    {
        let mut w = writer.lock().await;
        w.open().await.unwrap();
    }

    let start = Instant::now();
    let mut total_lock_wait = 0u64;

    for i in 0..100 {
        let lock_start = Instant::now();
        let mut w = writer.lock().await;
        total_lock_wait += lock_start.elapsed().as_millis() as u64;

        let data = vec![i as u8; 1024];
        w.write_at((i * 1024) as u64, &data).await.unwrap();
    }

    let duration = start.elapsed();
    Metrics::new(100 * 1024, duration.as_millis() as u64, 0, total_lock_wait)
        .with_label("lock_test")
}

async fn test_preallocation_performance(dir: &tempfile::TempDir) -> Metrics {
    let path = dir.path().join("prealloc_test.bin");
    let size = 10 * 1024 * 1024;

    let start = Instant::now();
    preallocate_file(&path, size, "falloc", false).await.unwrap();
    let duration = start.elapsed();

    Metrics::new(size, duration.as_millis() as u64, 0, 0)
        .with_label("preallocation")
}

async fn test_fsync_performance(dir: &tempfile::TempDir) -> Metrics {
    let path = dir.path().join("fsync_test.bin");
    let mut writer = CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();

    let start = Instant::now();
    for i in 0..100 {
        let data = vec![i as u8; 1024];
        writer.write_at((i * 1024) as u64, &data).await.unwrap();
        if i % 10 == 0 {
            writer.flush().await.unwrap();
        }
    }
    writer.flush().await.unwrap();
    let duration = start.elapsed();

    Metrics::new(100 * 1024, duration.as_millis() as u64, 0, 0)
        .with_label("fsync_test")
}

async fn test_throughput_performance(dir: &tempfile::TempDir) -> Metrics {
    let path = dir.path().join("throughput_test.bin");
    let mut writer = CachedDiskWriter::new(&path, None, None);
    writer.open().await.unwrap();

    let block_size = 64 * 1024;
    let num_blocks = 100;

    let start = Instant::now();
    for i in 0..num_blocks {
        let data = vec![i as u8; block_size];
        writer.write_at((i * block_size) as u64, &data).await.unwrap();
    }
    writer.flush().await.unwrap();
    let duration = start.elapsed();

    let throughput = (num_blocks * block_size) as f64 / duration.as_secs_f64();
    Metrics::new(throughput as u64, duration.as_millis() as u64, 0, 0)
        .with_label("throughput_test")
}
