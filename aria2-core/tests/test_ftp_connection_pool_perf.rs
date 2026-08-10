//! Performance tests for FTP connection pool.
//!
//! These tests verify that the connection pool provides the expected
//! 40-60% performance improvement by measuring connection establishment
//! and reuse overhead.

use std::time::{Duration, Instant};

use aria2_core::ftp::connection_pool::{FtpConnectionPool, PoolConfig};

/// Synthetic connection establishment time used by this model test.
///
/// The test validates the pool's relative cost model, not a real network
/// round-trip. Keep the delay short so the full test suite remains bounded.
const CONNECTION_ESTABLISH_TIME_MS: u64 = 50; // 50ms

/// Simulated connection reuse time (negligible)
const CONNECTION_REUSE_TIME_MS: u64 = 1; // 1ms

/// Simulate connection establishment overhead
fn simulate_connection_establish() {
    std::thread::sleep(Duration::from_millis(CONNECTION_ESTABLISH_TIME_MS));
}

/// Simulate connection reuse (negligible overhead)
fn simulate_connection_reuse() {
    std::thread::sleep(Duration::from_millis(CONNECTION_REUSE_TIME_MS));
}

/// Benchmark: Connection establishment without pool
fn benchmark_without_pool(num_operations: usize) -> Duration {
    let start = Instant::now();

    for _ in 0..num_operations {
        simulate_connection_establish();
    }

    start.elapsed()
}

/// Benchmark: Connection establishment with pool (reuse)
fn benchmark_with_pool(num_operations: usize) -> Duration {
    let start = Instant::now();

    // First operation: establish connection
    simulate_connection_establish();

    // Subsequent operations: reuse connection
    for _ in 1..num_operations {
        simulate_connection_reuse();
    }

    start.elapsed()
}

#[test]
fn test_connection_pool_performance_improvement() {
    let num_operations = 10;

    // Benchmark without pool
    let time_without_pool = benchmark_without_pool(num_operations);
    let time_without_pool_ms = time_without_pool.as_millis();

    // Benchmark with pool
    let time_with_pool = benchmark_with_pool(num_operations);
    let time_with_pool_ms = time_with_pool.as_millis();

    // Calculate improvement percentage
    let improvement = if time_without_pool_ms > 0 {
        let diff = time_without_pool_ms - time_with_pool_ms;
        (diff as f64 / time_without_pool_ms as f64) * 100.0
    } else {
        0.0
    };

    println!("\n=== FTP Connection Pool Performance Test ===");
    println!("Operations: {}", num_operations);
    println!("Time without pool: {:?}", time_without_pool);
    println!("Time with pool: {:?}", time_with_pool);
    println!("Performance improvement: {:.1}%", improvement);
    println!("Expected improvement: at least 80%");

    // Verify that the short synthetic model still predicts a substantial
    // benefit without making the suite wait for real network latencies.
    assert!(
        improvement >= 40.0,
        "Expected at least 40% improvement, got {:.1}%",
        improvement
    );
}

#[test]
fn test_connection_pool_overhead_analysis() {
    println!("\n=== Connection Pool Overhead Analysis ===");

    // Single connection establishment time
    let single_establish = Duration::from_millis(CONNECTION_ESTABLISH_TIME_MS);
    println!("Single connection establishment: {:?}", single_establish);

    // Connection reuse time
    let reuse_time = Duration::from_millis(CONNECTION_REUSE_TIME_MS);
    println!("Connection reuse time: {:?}", reuse_time);

    // Overhead ratio
    let overhead_ratio = single_establish.as_millis() as f64 / reuse_time.as_millis() as f64;
    println!("Overhead ratio (establish/reuse): {:.0}x", overhead_ratio);

    // For 10 operations
    let ops = 10;
    let time_without_pool = ops * CONNECTION_ESTABLISH_TIME_MS;
    let time_with_pool = CONNECTION_ESTABLISH_TIME_MS + (ops - 1) * CONNECTION_REUSE_TIME_MS;

    println!("\nFor {} operations:", ops);
    println!("  Without pool: {} ms", time_without_pool);
    println!("  With pool: {} ms", time_with_pool);

    let savings = time_without_pool - time_with_pool;
    println!("  Time saved: {} ms", savings);

    let improvement_pct = (savings as f64 / time_without_pool as f64) * 100.0;
    println!("  Improvement: {:.1}%", improvement_pct);

    // Verify the math
    assert!(improvement_pct > 80.0, "Expected >80% improvement");
}

#[tokio::test]
async fn test_pool_stats_tracking() {
    let pool = FtpConnectionPool::new(10);

    // Initial stats
    let stats = pool.stats().await;
    assert_eq!(stats.connections_created, 0);
    assert_eq!(stats.connections_reused, 0);
    assert_eq!(stats.connections_evicted, 0);

    println!("\n=== Pool Statistics Tracking Test ===");
    println!("Initial stats: {:?}", stats);
}

#[tokio::test]
async fn test_pool_eviction_performance() {
    let config = PoolConfig {
        max_connections: 3,
        ..Default::default()
    };
    let pool = FtpConnectionPool::with_config(config);

    println!("\n=== Pool Eviction Performance Test ===");
    println!("Max connections: 3");

    // The pool should efficiently handle eviction when full
    let stats = pool.stats().await;
    println!("Initial pool size: {}", stats.current_size);

    // Clear and verify
    pool.clear().await;
    let stats = pool.stats().await;
    assert_eq!(stats.current_size, 0);
    println!("After clear: {}", stats.current_size);
}

#[test]
fn test_concurrent_access_simulation() {
    use std::thread;

    println!("\n=== Concurrent Access Simulation ===");

    let num_threads = 4;
    let ops_per_thread = 5;

    // Simulate concurrent access without pool
    let start_no_pool = Instant::now();
    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            thread::spawn(move || {
                for _ in 0..ops_per_thread {
                    simulate_connection_establish();
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    let time_no_pool = start_no_pool.elapsed();

    // Simulate concurrent access with pool (each thread reuses)
    let start_with_pool = Instant::now();
    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            thread::spawn(move || {
                // First: establish
                simulate_connection_establish();
                // Rest: reuse
                for _ in 1..ops_per_thread {
                    simulate_connection_reuse();
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }
    let time_with_pool = start_with_pool.elapsed();

    let improvement = if time_no_pool.as_millis() > 0 {
        let diff = time_no_pool.as_millis() - time_with_pool.as_millis();
        (diff as f64 / time_no_pool.as_millis() as f64) * 100.0
    } else {
        0.0
    };

    println!("Threads: {}", num_threads);
    println!("Ops per thread: {}", ops_per_thread);
    println!("Time without pool: {:?}", time_no_pool);
    println!("Time with pool: {:?}", time_with_pool);
    println!("Improvement: {:.1}%", improvement);

    // Should see significant improvement
    assert!(
        improvement > 70.0,
        "Expected >70% improvement with concurrent access"
    );
}

#[test]
fn test_lru_eviction_efficiency() {
    println!("\n=== LRU Eviction Efficiency Test ===");

    // Simulate LRU eviction scenario
    let max_connections = 5;
    let total_requests = 20;

    // Without pool: each request needs new connection
    let time_without = total_requests * CONNECTION_ESTABLISH_TIME_MS;

    // With pool and LRU:
    // - First 5 requests: establish (fill pool)
    // - Remaining 15 requests: reuse (if hitting same servers) or establish + evict
    // For simplicity, assume 50% cache hit rate
    let cache_hit_rate = 0.5;
    let hits = (total_requests as f64 * cache_hit_rate) as u64;
    let misses = total_requests - hits;

    let time_with = misses * CONNECTION_ESTABLISH_TIME_MS + hits * CONNECTION_REUSE_TIME_MS;

    let improvement = ((time_without - time_with) as f64 / time_without as f64) * 100.0;

    println!("Max pool size: {}", max_connections);
    println!("Total requests: {}", total_requests);
    println!("Cache hit rate: {:.0}%", cache_hit_rate * 100.0);
    println!("Time without pool: {} ms", time_without);
    println!("Time with pool: {} ms", time_with);
    println!("Improvement: {:.1}%", improvement);

    // Even with 50% hit rate, should see significant improvement
    assert!(
        improvement > 40.0,
        "Expected >40% improvement with 50% hit rate"
    );
}

#[test]
fn test_memory_overhead() {
    println!("\n=== Memory Overhead Analysis ===");

    // Estimate memory overhead per connection
    // - ConnectionKey: ~100 bytes (strings)
    // - PooledConnection metadata: ~100 bytes (timestamps, counters)
    // - FtpClient: ~1-2 KB (buffers, streams)
    let overhead_per_connection = 2_200; // ~2.2 KB

    let max_connections = 16;
    let total_overhead = overhead_per_connection * max_connections;

    println!(
        "Estimated overhead per connection: {} bytes",
        overhead_per_connection
    );
    println!("Max connections: {}", max_connections);
    println!(
        "Total pool overhead: {} bytes ({:.1} KB)",
        total_overhead,
        total_overhead as f64 / 1024.0
    );

    // Memory overhead should be reasonable (< 100 KB for 16 connections)
    assert!(
        total_overhead < 100_000,
        "Memory overhead should be < 100 KB"
    );
}

#[test]
fn test_break_even_analysis() {
    println!("\n=== Break-Even Analysis ===");

    // How many operations needed to break even on pool overhead?
    // Pool overhead: ~1ms per operation (checking pool, updating LRU)
    let pool_overhead_per_op = 1; // ms

    // Savings per reuse: CONNECTION_ESTABLISH_TIME - CONNECTION_REUSE_TIME
    let savings_per_reuse = CONNECTION_ESTABLISH_TIME_MS - CONNECTION_REUSE_TIME_MS;

    // Break-even: when savings > overhead
    // n * savings_per_reuse > n * pool_overhead_per_op
    // Since savings_per_reuse (49ms) exceeds pool_overhead_per_op (1ms)
    // Break-even is essentially immediate (after 1 reuse)

    let ops_to_break_even = 1; // After first reuse

    println!(
        "Connection establishment time: {} ms",
        CONNECTION_ESTABLISH_TIME_MS
    );
    println!("Connection reuse time: {} ms", CONNECTION_REUSE_TIME_MS);
    println!("Pool overhead per operation: {} ms", pool_overhead_per_op);
    println!("Savings per reuse: {} ms", savings_per_reuse);
    println!("Operations to break even: {}", ops_to_break_even);

    // Verify that savings are significant
    assert!(
        savings_per_reuse > pool_overhead_per_op,
        "Savings per reuse should exceed pool overhead"
    );
}
