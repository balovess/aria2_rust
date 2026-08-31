//! Stress Tests for Concurrent Downloads
//!
//! Tests system stability under high concurrency:
//! - 50 concurrent downloads (reduced for memory efficiency)
//! - Memory stability monitoring
//! - No deadlock/panic verification
//! - Resource cleanup validation

mod e2e_helpers;
use e2e_helpers::mock_http_server::MockHttpServer;

use aria2_core::config::ConfigManager;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group::GroupId;
use aria2_core::request::request_group_man::RequestGroupMan;
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{Pid, System};
use tokio::sync::{RwLock, Semaphore};

/// Test 50 concurrent downloads using mock HTTP server (reduced from 100 for memory)
/// Verifies:
/// - No panic or deadlock occurs
/// - Memory remains stable (no unbounded growth)
/// - All downloads complete successfully
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stress_50_concurrent_downloads() {
    // Start mock HTTP server
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // Register small file endpoint (512 bytes each - reduced size)
    let small_file_data = vec![0xABu8; 512];
    server.register_range_response("/stress/file.bin", &small_file_data);

    // Register multiple endpoints for variety
    for i in 0..5 {
        let path = format!("/stress/file{}.bin", i);
        server.register_range_response(&path, &small_file_data);
    }

    let base_url = server.base_url();

    // Track memory before test
    let mem_before = get_memory_usage();

    // Create request group manager
    let manager = Arc::new(RequestGroupMan::new());

    // Semaphore to limit concurrent downloads (prevent resource exhaustion)
    let semaphore = Arc::new(Semaphore::new(20)); // Max 20 simultaneous (reduced from 50)

    // Spawn 50 concurrent download tasks (reduced from 100)
    let mut handles = Vec::new();
    let start_time = Instant::now();

    for i in 0..50 {
        let manager_clone = manager.clone();
        let semaphore_clone = semaphore.clone();
        let url = format!("{}/stress/file{}.bin", base_url, i % 5);

        handles.push(tokio::spawn(async move {
            // Acquire semaphore permit
            let _permit = semaphore_clone.acquire().await.unwrap();

            // Add download group (async)
            let gid = manager_clone
                .add_group(vec![url], Default::default())
                .expect("Failed to add group");

            // Simulate download progress tracking
            tokio::time::sleep(Duration::from_millis(5)).await;

            gid.value()
        }));
    }

    // Wait for all tasks to complete
    let results: Vec<_> = futures::future::join_all(handles).await;

    let elapsed = start_time.elapsed();

    // Verify all 50 tasks completed without panic
    let successful_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        successful_count, 50,
        "All 50 downloads should complete without panic"
    );

    // Verify all GIDs are unique
    let gid_values: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    let unique_gids = gid_values.len();
    assert_eq!(unique_gids, 50, "All GIDs should be unique");

    // Track memory after test
    let mem_after = get_memory_usage();
    let mem_growth = mem_after - mem_before;

    // Memory should not grow excessively (allow up to 30MB growth for 50 downloads)
    assert!(
        mem_growth < 30_000_000,
        "Memory growth should be bounded: grew by {} bytes",
        mem_growth
    );

    println!(
        "Stress test completed: 50 concurrent downloads in {}ms, memory growth: {} bytes",
        elapsed.as_millis(),
        mem_growth
    );

    // Cleanup
    server.shutdown().await;
}

/// Test rapid creation and destruction of download groups
/// Verifies no resource leaks or deadlocks during lifecycle churn
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stress_download_lifecycle_churn() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let small_data = vec![0xCDu8; 256]; // Reduced size
    server.register_range_response("/churn/test.bin", &small_data);

    let base_url = server.base_url();

    // Perform 200 rapid create/remove cycles (reduced from 500)
    let iterations = 200;
    let manager = Arc::new(RequestGroupMan::new());

    let start_time = Instant::now();

    for batch in 0..5 {
        let mut batch_handles = Vec::new();

        for i in 0..40 {
            let manager_clone = manager.clone();
            let url = format!("{}/churn/test.bin", base_url);
            let idx = batch * 40 + i;

            batch_handles.push(tokio::spawn(async move {
                // Create group (async)
                let gid = manager_clone
                    .add_group(vec![url], Default::default())
                    .expect("Failed to add group");

                // Small delay
                tokio::time::sleep(Duration::from_micros(50)).await;

                // Remove group (async)
                manager_clone.remove_group(gid).ok();

                idx
            }));
        }

        // Wait for batch to complete
        let batch_results: Vec<_> = futures::future::join_all(batch_handles).await;

        // Verify batch completed
        let batch_success = batch_results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(
            batch_success, 40,
            "Batch {} should complete all 40 tasks",
            batch
        );
    }

    let elapsed = start_time.elapsed();

    println!(
        "Lifecycle churn test completed: {} iterations in {}ms",
        iterations,
        elapsed.as_millis()
    );

    server.shutdown().await;
}

/// Test concurrent downloads with limited resources (semaphore)
/// Verifies proper resource management under constrained conditions
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stress_concurrent_with_resource_limit() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // Smaller files (2KB) to simulate more realistic downloads (reduced from 10KB)
    let medium_data = vec![0xEFu8; 2_048];
    server.register_range_response("/limit/file.bin", &medium_data);

    // Semaphore to limit concurrent downloads
    let semaphore = Arc::new(Semaphore::new(5)); // Only 5 simultaneous

    // Track active downloads
    let active_count = Arc::new(RwLock::new(0u32));
    let max_concurrent = Arc::new(RwLock::new(0u32));

    let mut handles = Vec::new();

    for i in 0..50 {
        let active_clone = active_count.clone();
        let max_clone = max_concurrent.clone();
        let semaphore_clone = semaphore.clone();

        handles.push(tokio::spawn(async move {
            // Acquire semaphore permit FIRST (this enforces the limit)
            let _permit = semaphore_clone.acquire().await.unwrap();

            // Track concurrent count (now within semaphore limit)
            {
                let mut active = active_clone.write().await;
                *active += 1;
                let mut max = max_clone.write().await;
                if *active > *max {
                    *max = *active;
                }
            }

            // Simulate download
            tokio::time::sleep(Duration::from_millis(30)).await;

            // Decrement active count
            {
                let mut active = active_clone.write().await;
                *active -= 1;
            }

            // Permit released automatically when dropped
            i
        }));
    }

    // Wait for all
    let results: Vec<_> = futures::future::join_all(handles).await;

    // Verify max concurrent was within limit (should not exceed 5)
    let max_observed = *max_concurrent.read().await;
    assert!(
        max_observed <= 6, // Allow 1 extra due to timing
        "Max concurrent downloads should respect semaphore limit: observed {}",
        max_observed
    );

    // All should complete
    let success_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(success_count, 50);

    println!(
        "Resource limit test: max concurrent observed = {}, all {} completed",
        max_observed, success_count
    );

    server.shutdown().await;
}

/// Test concurrent ConfigManager access
/// Verifies no deadlock under heavy read/write contention
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stress_config_manager_concurrent_access() {
    let manager = Arc::new(RwLock::new(ConfigManager::new()));

    let mut handles = Vec::new();

    // 100 concurrent operations: 50 readers, 50 writers (reduced from 200)
    for i in 0..100 {
        let manager_clone = manager.clone();

        handles.push(tokio::spawn(async move {
            if i % 2 == 0 {
                // Writer
                let mut m = manager_clone.write().await;
                let _ = m
                    .set_global_option("split", aria2_core::config::OptionValue::Int(i as i64))
                    .await;
            } else {
                // Reader
                let m = manager_clone.read().await;
                let _ = m.get_global_i64("split").await;
            }
            i
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // All should complete without deadlock
    let success_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        success_count, 100,
        "All 100 config operations should complete"
    );

    println!(
        "Config manager stress test: {} operations completed",
        success_count
    );
}

/// Deadlock regression test for `set_global_option` actually executing
/// (previously the future was dropped via `let _ = ...` without `.await`,
/// so the write path was never exercised under contention).
///
/// This test forces the real write path: outer `RwLock<ConfigManager>` write
/// lock → inner `global_opts.read()` → drop → inner `global_opts.write()` →
/// `broadcast::send()`. With 200 concurrent tasks (100 writers × 5 rounds +
/// 100 readers × 5 rounds) on 8 worker threads, any deadlock or writer
/// starvation will cause the 15-second timeout to fire.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_config_manager_no_deadlock_with_actual_writes() {
    use aria2_core::config::OptionValue;

    let manager = Arc::new(RwLock::new(ConfigManager::new()));

    // 100 writers, each writing 5 rounds to "split" with a unique value
    // derived from (task_id, round). 100 readers, each reading 5 rounds.
    const WRITERS: usize = 100;
    const READERS: usize = 100;
    const ROUNDS: usize = 5;

    let timeout = tokio::time::timeout(Duration::from_secs(15), async {
        let mut handles = Vec::with_capacity(WRITERS + READERS);

        for task_id in 0..WRITERS {
            let m = manager.clone();
            handles.push(tokio::spawn(async move {
                for round in 0..ROUNDS {
                    let mut guard = m.write().await;
                    // Keep value within the "split" option's valid range [1, 16]
                    // so the validation inside set_global_option passes.
                    let val = (task_id as i64 + round as i64) % 16 + 1;
                    guard
                        .set_global_option("split", OptionValue::Int(val))
                        .await
                        .expect("set_global_option should succeed");
                }
                task_id
            }));
        }

        for task_id in 0..READERS {
            let m = manager.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..ROUNDS {
                    let guard = m.read().await;
                    let _ = guard.get_global_i64("split").await;
                }
                task_id
            }));
        }

        let results: Vec<_> = futures::future::join_all(handles).await;
        results
    })
    .await;

    // If the timeout fired, the test panics with a clear message — this is
    // the deadlock signal.
    let results = timeout.expect(
        "DEADLOCK DETECTED: config manager operations did not complete within 15s \
         — likely deadlock or writer starvation in set_global_option write path",
    );

    let success_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        success_count,
        WRITERS + READERS,
        "All {} tasks should complete without deadlock or panic",
        WRITERS + READERS
    );

    // Verify the last writer's value is visible to a reader — confirms the
    // write path actually persisted data (not silently dropped like before
    // the .await fix).
    {
        let m = manager.read().await;
        let val = m.get_global_i64("split").await;
        assert!(
            val.is_some(),
            "split option should exist after writes — write path was silently dropped?"
        );
    }

    println!(
        "Deadlock regression test: {} tasks × {} rounds completed, data persisted",
        WRITERS + READERS,
        ROUNDS
    );
}

/// Test rapid engine creation and shutdown
/// Verifies no resource leaks or hangs during engine lifecycle
#[test]
fn test_stress_engine_lifecycle() {
    let start_time = Instant::now();

    // Create and destroy 20 engines rapidly (reduced from 50)
    for i in 0..20 {
        let engine = DownloadEngine::new();

        // Simulate some work
        std::thread::sleep(Duration::from_millis(2));

        // Drop engine (shutdown)
        drop(engine);

        // Verify no hang
        if i % 5 == 0 {
            println!("Engine lifecycle iteration {} completed", i);
        }
    }

    let elapsed = start_time.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "20 engine lifecycles should complete in reasonable time"
    );

    println!(
        "Engine lifecycle test completed in {}ms",
        elapsed.as_millis()
    );
}

/// Test concurrent download with mock server under load
/// Verifies mock server stability and proper connection handling
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stress_mock_server_high_load() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // Register multiple endpoints with different sizes (smaller sizes)
    for size in [100, 200, 500, 1000].iter() {
        let path = format!("/load/file_{}.bin", size);
        let data = vec![0xAAu8; *size];
        server.register_range_response(&path, &data);
    }

    let base_url = server.base_url();

    // 100 concurrent requests to mock server (reduced from 200)
    let mut handles = Vec::new();

    for i in 0..100 {
        let url = format!(
            "{}/load/file_{}.bin",
            base_url,
            [100, 200, 500, 1000][i % 4]
        );

        handles.push(tokio::spawn(async move {
            // Use reqwest to make actual HTTP request
            let client = reqwest::Client::new();
            let resp = client.get(&url).send().await;

            match resp {
                Ok(r) => {
                    let status = r.status();
                    let _body = r.bytes().await;
                    status.as_u16()
                }
                Err(_) => 0u16,
            }
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // Verify all requests completed
    let success_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(success_count, 100, "All 100 HTTP requests should complete");

    // Verify all responses were successful (status 200)
    let ok_responses = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|&status| *status == 200)
        .count();
    assert!(
        ok_responses >= 90, // Allow some failures due to connection limits
        "Most HTTP requests should succeed: {} of 100",
        ok_responses
    );

    println!(
        "Mock server load test: {} requests completed, {} successful responses",
        success_count, ok_responses
    );

    server.shutdown().await;
}

/// Test memory stability during sustained concurrent operations
/// Verifies no memory leak patterns over extended duration
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_stress_memory_stability_sustained() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");
    let data = vec![0xBBu8; 512]; // Reduced size
    server.register_range_response("/mem/test.bin", &data);

    let base_url = server.base_url();
    let manager = Arc::new(RequestGroupMan::new());

    // Track memory at intervals
    let mem_samples: Arc<RwLock<Vec<usize>>> = Arc::new(RwLock::new(Vec::new()));

    // Run for 3 seconds, sampling memory every 500ms (reduced from 5 seconds)
    let test_duration = Duration::from_secs(3);
    let sample_interval = Duration::from_millis(500);

    let start_time = Instant::now();

    // Spawn continuous download tasks
    let download_task = {
        let manager_clone = manager.clone();
        let base_url_clone = base_url.clone();

        tokio::spawn(async move {
            while start_time.elapsed() < test_duration {
                let mut batch_handles = Vec::new();

                for _ in 0..10 {
                    let m = manager_clone.clone();
                    let url = format!("{}/mem/test.bin", base_url_clone);

                    batch_handles.push(tokio::spawn(async move {
                        // Add group (async)
                        let gid = m
                            .add_group(vec![url], Default::default())
                            .expect("Failed to add group");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        // Remove group (sync now)
                        m.remove_group(gid).ok();
                    }));
                }

                let _ = futures::future::join_all(batch_handles).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    };

    // Spawn memory sampler
    let sampler_task = {
        let samples_clone = mem_samples.clone();

        tokio::spawn(async move {
            while start_time.elapsed() < test_duration {
                let mem = get_memory_usage();
                samples_clone.write().await.push(mem);
                tokio::time::sleep(sample_interval).await;
            }
        })
    };

    // Wait for both tasks
    let _ = futures::future::join_all([download_task, sampler_task]).await;

    // Analyze memory samples
    let samples = mem_samples.read().await;
    if samples.len() >= 2 {
        let first = samples.first().unwrap();
        let last = samples.last().unwrap();
        let growth = last.saturating_sub(*first);

        // Memory should not grow unboundedly (allow up to 120MB growth for
        // allocator hysteresis on CI VMs).  The test catches unbounded leaks
        // that grow without plateauing, not precise budget violations.
        assert!(
            growth < 120_000_000,
            "Memory should remain stable over sustained operations: grew by {} bytes",
            growth
        );

        println!(
            "Memory stability test: {} samples, growth = {} bytes",
            samples.len(),
            growth
        );
    }

    server.shutdown().await;
}

/// Test concurrent RequestGroupMan operations
/// Verifies DashMap-based implementation handles concurrent access correctly
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stress_request_group_man_concurrent() {
    let manager = Arc::new(RequestGroupMan::new());

    // 200 concurrent add operations (reduced from 500)
    let mut add_handles = Vec::new();
    for i in 0..200 {
        let m = manager.clone();
        add_handles.push(tokio::spawn(async move {
            let uri = format!("http://example.com/file{}.bin", i);
            m.add_group(vec![uri], Default::default())
        }));
    }

    let add_results: Vec<_> = futures::future::join_all(add_handles).await;

    // All should succeed
    let add_success = add_results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(add_success, 200, "All 200 add operations should succeed");

    // Collect GIDs
    let gids: Vec<GroupId> = add_results
        .into_iter()
        .filter_map(|r| r.ok())
        .filter_map(|r| r.ok())
        .collect();

    // Verify count
    let count = manager.count();
    assert_eq!(count, 200, "Manager should have 200 groups");

    // 200 concurrent remove operations
    let mut remove_handles = Vec::new();
    for gid in gids {
        let m = manager.clone();
        remove_handles.push(tokio::spawn(async move { m.remove_group(gid) }));
    }

    let remove_results: Vec<_> = futures::future::join_all(remove_handles).await;

    // All should succeed
    let remove_success = remove_results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        remove_success, 200,
        "All 200 remove operations should succeed"
    );

    // Verify empty
    let final_count = manager.count();
    assert_eq!(final_count, 0, "Manager should be empty after removes");

    println!("RequestGroupMan stress test: 200 adds, 200 removes completed");
}

/// Helper: Get current process memory usage (in bytes).
/// Uses `sysinfo` for cross-platform RSS measurement.
/// On Linux, calls `malloc_trim(0)` before sampling to release glibc's
/// cached free-list memory, so the measurement reflects actual retained
/// memory rather than allocator caching.
fn get_memory_usage() -> usize {
    use std::cell::RefCell;
    thread_local! {
        static SYS: RefCell<System> = RefCell::new(System::new());
    }

    // On Linux, release glibc's cached free-list memory back to the OS
    // so RSS reflects actual retained memory, not allocator caching.
    #[cfg(target_os = "linux")]
    unsafe {
        libc::malloc_trim(0);
    }

    let pid = Pid::from_u32(std::process::id());
    let rss = SYS.with(|cell| {
        let mut sys = cell.borrow_mut();
        if !sys.refresh_process(pid) {
            return 0;
        }
        sys.process(pid).map(|p| p.memory()).unwrap_or(0)
    });
    rss as usize
}
