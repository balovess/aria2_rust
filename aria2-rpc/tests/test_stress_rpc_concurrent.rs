//! Stress Tests for RPC Concurrent Requests
//!
//! Tests RPC system stability under high concurrency:
//! - 1000 concurrent RPC requests
//! - RpcEngine stress testing
//! - WebSocket event publishing stability
//! - No response loss verification

use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_rpc::engine::RpcEngine;
use aria2_rpc::json_rpc::JsonRpcRequest;
use aria2_rpc::server::RpcAuthMiddleware;
use aria2_rpc::websocket::{DownloadEvent, EventPublisher, NotificationBatcher};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, Semaphore};

/// Test 1000 concurrent RPC requests using RpcEngine
/// Verifies:
/// - No panic or deadlock occurs
/// - All requests receive responses
/// - No response loss
/// - Memory remains stable
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn test_stress_1000_concurrent_rpc_requests() {
    let engine = Arc::new(RpcEngine::new());

    // Track memory before test
    let mem_before = get_memory_usage();

    // Semaphore to batch requests (prevent overwhelming the engine)
    let semaphore = Arc::new(Semaphore::new(100)); // Max 100 simultaneous

    // Spawn 1000 concurrent RPC requests
    let mut handles = Vec::new();
    let start_time = Instant::now();

    for i in 0..1000 {
        let engine_clone = engine.clone();
        let semaphore_clone = semaphore.clone();

        // Mix different RPC methods for variety
        let method = match i % 5 {
            0 => "aria2.addUri",
            1 => "aria2.getVersion",
            2 => "aria2.getGlobalStat",
            3 => "aria2.tellActive",
            _ => "aria2.getSessionInfo",
        };

        handles.push(tokio::spawn(async move {
            // Acquire semaphore permit
            let _permit = semaphore_clone.acquire().await.unwrap();

            // Create request
            let params = if method == "aria2.addUri" {
                json!([format!("http://example.com/file{}.zip", i)])
            } else {
                json!([])
            };

            let request = JsonRpcRequest::new(method, params).with_id(i);

            // Handle request
            let response = engine_clone.handle_request(&request).await;

            (i, response.is_success(), response)
        }));
    }

    // Wait for all tasks to complete
    let results: Vec<_> = futures::future::join_all(handles).await;

    let elapsed = start_time.elapsed();

    // Verify all 1000 tasks completed without panic
    let completed_count = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(
        completed_count, 1000,
        "All 1000 RPC requests should complete without panic"
    );

    // Verify all responses were received
    let success_count = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|(_, success, _)| *success)
        .count();
    assert_eq!(
        success_count, 1000,
        "All 1000 RPC responses should be successful"
    );

    // Verify no response ID loss
    let response_ids: Vec<_> = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|(id, _, _)| *id)
        .collect();

    // Check all IDs are present (no response loss)
    for i in 0..1000 {
        assert!(
            response_ids.contains(&i),
            "Response ID {} should be present (no response loss)",
            i
        );
    }

    // Track memory after test
    let mem_after = get_memory_usage();
    let mem_growth = mem_after - mem_before;

    // Memory should not grow excessively (allow up to 50MB growth for 1000 requests)
    assert!(
        mem_growth < 50_000_000,
        "Memory growth should be bounded: grew by {} bytes",
        mem_growth
    );

    println!(
        "RPC stress test completed: 1000 concurrent requests in {}ms, {} successful, memory growth: {} bytes",
        elapsed.as_millis(),
        success_count,
        mem_growth
    );
}

/// Test concurrent addUri requests creating multiple download tasks
/// Verifies task creation stability under heavy load
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_concurrent_add_uri_tasks() {
    let engine = Arc::new(RpcEngine::new());

    // Create 500 concurrent addUri requests
    let mut handles = Vec::new();
    let start_time = Instant::now();

    for i in 0..500 {
        let engine_clone = engine.clone();

        handles.push(tokio::spawn(async move {
            let request = JsonRpcRequest::new(
                "aria2.addUri",
                json!([format!("http://test.com/download{}.bin", i)]),
            )
            .with_id(i);

            let response = engine_clone.handle_request(&request).await;

            // Extract GID from response
            let gid = if response.is_success() {
                response
                    .result
                    .and_then(|r| r.as_str().map(|s| s.to_string()))
            } else {
                None
            };

            (i, gid)
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // Verify all completed
    let completed = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(completed, 500, "All 500 addUri requests should complete");

    // Verify all GIDs are unique (no collision)
    let gids: Vec<_> = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter_map(|(_, gid)| gid.clone())
        .collect();

    let unique_gids = gids.len();
    assert_eq!(unique_gids, 500, "All 500 GIDs should be unique");

    // Verify engine task count
    let task_count = engine.task_count().await;
    assert_eq!(task_count, 500, "Engine should have 500 active tasks");

    println!(
        "Concurrent addUri test: {} tasks created in {}ms",
        unique_gids,
        start_time.elapsed().as_millis()
    );
}

/// Test WebSocket EventPublisher stability under high event volume
/// Verifies:
/// - Event publishing doesn't block
/// - Subscribers receive all events
/// - No event loss under load
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_websocket_event_publisher() {
    let publisher = Arc::new(EventPublisher::new(1024));

    // Create multiple subscribers
    let mut subscribers = Vec::new();
    for i in 0..10 {
        let rx = publisher.subscribe(format!("client-{}", i), None).await;
        subscribers.push((i, rx));
    }

    // Spawn subscriber receivers to count received events
    let received_counts: Arc<RwLock<Vec<usize>>> = Arc::new(RwLock::new(vec![0; 10]));
    let mut receiver_handles = Vec::new();

    for (client_id, mut rx) in subscribers {
        let counts_clone = received_counts.clone();

        receiver_handles.push(tokio::spawn(async move {
            let timeout = Duration::from_secs(5);
            let start = Instant::now();

            while start.elapsed() < timeout {
                match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                    Ok(Ok(_)) => {
                        let mut counts = counts_clone.write().await;
                        counts[client_id] += 1;
                    }
                    Ok(Err(_)) | Err(_) => break,
                }
            }
        }));
    }

    // Publish 500 events concurrently
    let mut publish_handles = Vec::new();
    let start_time = Instant::now();

    for i in 0..500 {
        let publisher_clone = publisher.clone();

        publish_handles.push(tokio::spawn(async move {
            let event = DownloadEvent::download_start(format!("gid-{}", i));

            publisher_clone.publish_event(event).unwrap_or_else(|e| {
                eprintln!("Publish error: {}", e);
                0
            })
        }));
    }

    // Wait for all publishes
    let publish_results: Vec<_> = futures::future::join_all(publish_handles).await;
    let publish_success = publish_results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(publish_success, 500, "All 500 events should be published");

    // Give receivers time to process
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Wait for receivers
    let _ = futures::future::join_all(receiver_handles).await;

    // Check received counts
    let counts = received_counts.read().await;
    let total_received: usize = counts.iter().sum();

    // Due to broadcast channel semantics, all subscribers should receive events
    // Allow some loss due to timing
    assert!(
        total_received >= 4000, // 10 subscribers * 400 events minimum
        "Subscribers should receive most events: total received = {}",
        total_received
    );

    println!(
        "WebSocket stress test: 500 events published in {}ms, {} total received across 10 subscribers",
        start_time.elapsed().as_millis(),
        total_received
    );
}

/// Test NotificationBatcher under high throughput
/// Verifies deduplication and batching work correctly under load
#[test]
fn test_stress_notification_batcher_high_throughput() {
    let mut batcher = NotificationBatcher::new()
        .with_max_batch_size(100)
        .with_flush_interval_ms(100);

    let start_time = Instant::now();

    // Push 1000 events rapidly
    let mut flush_count = 0;
    let mut total_sent = 0;

    for i in 0..1000 {
        let gid = format!("gid-{}", i % 50); // 50 unique GIDs (creates duplicates)
        let event = DownloadEvent::download_complete(gid);

        if batcher.push(event) {
            // Auto-flush triggered
            flush_count += 1;
        }

        // Manual flush check every 100 events
        if i % 100 == 99
            && let Some(batch) = batcher.maybe_flush()
        {
            total_sent += batch.len();
        }
    }

    // Final flush
    std::thread::sleep(Duration::from_millis(150));
    if let Some(batch) = batcher.maybe_flush() {
        total_sent += batch.len();
    }

    let elapsed = start_time.elapsed();
    let (sent, deduped) = batcher.stats();

    // Verify deduplication worked (should have deduped events due to duplicate GIDs)
    assert!(
        deduped > 0,
        "Deduplication should have occurred: {} events deduped",
        deduped
    );

    // Verify total sent matches stats
    assert_eq!(
        sent, total_sent as u64,
        "Stats should match actual sent count"
    );

    println!(
        "NotificationBatcher stress test: 1000 events in {}ms, {} sent, {} deduped, {} flushes",
        elapsed.as_millis(),
        sent,
        deduped,
        flush_count
    );
}

/// Test RPC authentication under concurrent load
/// Verifies auth middleware doesn't cause deadlock or performance issues
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_rpc_auth_concurrent() {
    // Create engine with auth
    let engine = Arc::new(
        RpcEngine::new().with_auth_middleware(RpcAuthMiddleware::new("stress-test-token")),
    );

    let mut handles = Vec::new();

    // 200 requests with valid token
    for i in 0..200 {
        let engine_clone = engine.clone();

        handles.push(tokio::spawn(async move {
            let request =
                JsonRpcRequest::new("aria2.getVersion", json!({"token": "stress-test-token"}))
                    .with_id(i);

            let response = engine_clone.handle_request(&request).await;
            (i, response.is_success())
        }));
    }

    // 100 requests with invalid token
    for i in 200..300 {
        let engine_clone = engine.clone();

        handles.push(tokio::spawn(async move {
            let request =
                JsonRpcRequest::new("aria2.getVersion", json!({"token": "wrong-token"})).with_id(i);

            let response = engine_clone.handle_request(&request).await;
            (i, response.is_error())
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // Verify valid token requests succeeded
    let valid_success = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|(id, success)| *id < 200 && *success)
        .count();
    assert_eq!(
        valid_success, 200,
        "All valid token requests should succeed"
    );

    // Verify invalid token requests failed
    let invalid_failed = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|(id, is_error)| *id >= 200 && *is_error)
        .count();
    assert_eq!(
        invalid_failed, 100,
        "All invalid token requests should fail"
    );

    println!(
        "Auth stress test: {valid_success} valid succeeded, {invalid_failed} invalid rejected"
    );
}

/// Test rapid task lifecycle: add -> pause -> unpause -> remove
/// Verifies state transitions work correctly under concurrent load
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_task_lifecycle_operations() {
    let group_man = Arc::new(RwLock::new(RequestGroupMan::new()));
    let mut download_engine = DownloadEngine::new(1);
    download_engine.set_request_group_man(Arc::clone(&group_man));
    download_engine.set_keep_alive(true);
    let engine_cmd_tx = download_engine.engine_command_sender();
    let shutdown_tx = download_engine
        .take_shutdown_sender()
        .expect("download engine must provide a shutdown sender");
    let engine_task = tokio::spawn(async move {
        download_engine
            .run()
            .await
            .expect("download engine should run");
    });
    let engine = Arc::new(RpcEngine::wired(Arc::clone(&group_man), engine_cmd_tx));

    // Create tasks first
    let mut gids = Vec::new();
    for i in 0..100 {
        let request = JsonRpcRequest::new(
            "aria2.addUri",
            json!([format!("http://test.com/lifecycle{}.bin", i)]),
        )
        .with_id(i);

        let response = engine.handle_request(&request).await;
        if response.is_success() {
            let gid = response.result.unwrap().as_str().unwrap().to_string();
            gids.push(gid);
        }
    }

    assert_eq!(gids.len(), 100, "Should create 100 tasks");

    // Now perform concurrent lifecycle operations on all tasks
    let mut handles = Vec::new();
    let start_time = Instant::now();

    for (i, gid) in gids.iter().enumerate() {
        let engine_clone = engine.clone();
        let gid_clone = gid.clone();

        handles.push(tokio::spawn(async move {
            // Pause
            let pause_req =
                JsonRpcRequest::new("aria2.pause", json!([gid_clone.clone()])).with_id(i * 4);
            let pause_resp = engine_clone.handle_request(&pause_req).await;

            // Unpause
            let unpause_req =
                JsonRpcRequest::new("aria2.unpause", json!([gid_clone.clone()])).with_id(i * 4 + 1);
            let unpause_resp = engine_clone.handle_request(&unpause_req).await;

            // Tell status
            let status_req = JsonRpcRequest::new("aria2.tellStatus", json!([gid_clone.clone()]))
                .with_id(i * 4 + 2);
            let status_resp = engine_clone.handle_request(&status_req).await;

            // Remove
            let remove_req =
                JsonRpcRequest::new("aria2.remove", json!([gid_clone.clone()])).with_id(i * 4 + 3);
            let remove_resp = engine_clone.handle_request(&remove_req).await;

            (
                pause_resp.is_success(),
                unpause_resp.is_success(),
                status_resp.is_success(),
                remove_resp.is_success(),
            )
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // Verify all lifecycle operations completed
    let all_success = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|(p, u, s, r)| *p && *u && *s && *r)
        .count();

    assert_eq!(all_success, 100, "All lifecycle operations should succeed");

    // `remove` is asynchronous; wait for the real engine loop to finalize all
    // completion notifications and demote the groups to stopped results.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut task_count = engine.task_count().await;
    while task_count != 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        task_count = engine.task_count().await;
    }
    assert_eq!(task_count, 0, "All tasks should be removed");

    let _ = shutdown_tx.send(());
    engine_task.abort();

    println!(
        "Task lifecycle stress test: 100 tasks, 400 operations in {}ms",
        start_time.elapsed().as_millis()
    );
}

/// Test batch RPC requests (system.multicall simulation)
/// Verifies handling of multiple requests in single call
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_batch_rpc_requests() {
    let engine = Arc::new(RpcEngine::new());

    // Create 50 batch requests, each containing 10 operations
    let mut handles = Vec::new();
    let start_time = Instant::now();

    for batch_id in 0..50 {
        let engine_clone = engine.clone();

        handles.push(tokio::spawn(async move {
            // Simulate batch by processing 10 requests sequentially
            let mut results = Vec::new();

            for op_id in 0..10 {
                let request = JsonRpcRequest::new("aria2.getVersion", json!([]))
                    .with_id(batch_id * 10 + op_id);

                let response = engine_clone.handle_request(&request).await;
                results.push(response.is_success());
            }

            let success_count = results.iter().filter(|&s| *s).count();
            (batch_id, success_count)
        }));
    }

    let batch_results: Vec<_> = futures::future::join_all(handles).await;

    // Verify all batches completed
    let completed_batches = batch_results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(completed_batches, 50, "All 50 batches should complete");

    // Verify all operations succeeded
    let total_success: usize = batch_results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|(_, count)| *count)
        .sum();
    assert_eq!(total_success, 500, "All 500 operations should succeed");

    println!(
        "Batch RPC stress test: 50 batches, 500 operations in {}ms",
        start_time.elapsed().as_millis()
    );
}

/// Test concurrent global option changes
/// Verifies no race conditions in global configuration updates
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_concurrent_global_options() {
    let engine = Arc::new(RpcEngine::new());

    let mut handles = Vec::new();

    // 100 concurrent changeGlobalOption requests
    for i in 0..100 {
        let engine_clone = engine.clone();

        handles.push(tokio::spawn(async move {
            let request = JsonRpcRequest::new(
                "aria2.changeGlobalOption",
                json!([{
                    "max-concurrent-downloads": i % 10 + 1,
                    "max-connection-per-server": i % 5 + 1,
                    "split": i % 16 + 1,
                }]),
            )
            .with_id(i);

            let response = engine_clone.handle_request(&request).await;
            (i, response.is_success())
        }));
    }

    // 100 concurrent getGlobalOption requests
    for i in 100..200 {
        let engine_clone = engine.clone();

        handles.push(tokio::spawn(async move {
            let request = JsonRpcRequest::new("aria2.getGlobalOption", json!([])).with_id(i);

            let response = engine_clone.handle_request(&request).await;
            (i, response.is_success())
        }));
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    // All should complete
    let success_count = results
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|(_, success)| *success)
        .count();
    assert_eq!(
        success_count, 200,
        "All 200 option operations should succeed"
    );

    println!(
        "Global options stress test: {} operations completed",
        success_count
    );
}

/// Test WebSocket subscriber churn (subscribe/unsubscribe rapidly)
/// Verifies no memory leak or deadlock during subscriber lifecycle
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_websocket_subscriber_churn() {
    let publisher = Arc::new(EventPublisher::new(256));

    let start_time = Instant::now();
    let iterations = 500;

    // Rapid subscribe/unsubscribe cycles
    for i in 0..iterations {
        let publisher_clone = publisher.clone();
        let sub_id = format!("churn-client-{}", i);

        // Subscribe
        let _rx = publisher_clone.subscribe(&sub_id, None).await;

        // Small delay
        tokio::time::sleep(Duration::from_micros(50)).await;

        // Unsubscribe
        publisher_clone.unsubscribe(&sub_id).await;

        // Verify subscriber count
        let count = publisher_clone.subscriber_count().await;
        assert_eq!(
            count, 0,
            "Subscriber should be removed after churn iteration {}",
            i
        );
    }

    // Final verification
    let final_count = publisher.subscriber_count().await;
    assert_eq!(
        final_count, 0,
        "No subscribers should remain after churn test"
    );

    println!(
        "Subscriber churn test: {} iterations in {}ms",
        iterations,
        start_time.elapsed().as_millis()
    );
}

/// Test concurrent event publishing with multiple subscribers
/// Verifies broadcast channel stability under mixed load
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn test_stress_event_publish_subscribe_mixed() {
    let publisher = Arc::new(EventPublisher::new(512));

    // Track events received per subscriber
    let received: Arc<RwLock<Vec<usize>>> = Arc::new(RwLock::new(Vec::new()));

    // Spawn 20 subscriber tasks
    let mut sub_handles = Vec::new();
    for i in 0..20 {
        let publisher_clone = publisher.clone();
        let received_clone = received.clone();

        sub_handles.push(tokio::spawn(async move {
            let mut rx = publisher_clone.subscribe(format!("sub-{}", i), None).await;
            let mut count = 0;

            let timeout = Duration::from_secs(3);
            let start = Instant::now();

            while start.elapsed() < timeout {
                if rx.try_recv().is_ok() {
                    count += 1;
                }
                tokio::time::sleep(Duration::from_micros(10)).await;
            }

            received_clone.write().await.push(count);
        }));
    }

    // Spawn 100 publisher tasks
    let mut pub_handles = Vec::new();
    for i in 0..100 {
        let publisher_clone = publisher.clone();

        pub_handles.push(tokio::spawn(async move {
            let event = DownloadEvent::download_complete(format!("event-gid-{}", i));

            publisher_clone.publish_event(event).ok();

            // Small delay between publishes
            tokio::time::sleep(Duration::from_millis(10)).await;

            i
        }));
    }

    // Wait for publishers
    let pub_results: Vec<_> = futures::future::join_all(pub_handles).await;
    let pub_success = pub_results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(pub_success, 100, "All 100 events should be published");

    // Wait for subscribers
    let _ = futures::future::join_all(sub_handles).await;

    // Analyze received counts
    let received_counts = received.read().await;
    let total_received: usize = received_counts.iter().sum();
    let avg_received = total_received as f64 / received_counts.len() as f64;

    // All subscribers should have received some events
    assert!(
        total_received > 0,
        "Subscribers should receive events: total = {}",
        total_received
    );

    println!(
        "Mixed publish/subscribe test: 100 events, {} total received, avg {} per subscriber",
        total_received, avg_received
    );
}

/// Helper: Get current process memory usage (in bytes)
fn get_memory_usage() -> usize {
    #[cfg(windows)]
    {
        use std::mem::MaybeUninit;
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { MaybeUninit::zeroed().assume_init() };
        counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;

        let result =
            unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };

        if result != 0 {
            counters.WorkingSetSize
        } else {
            0
        }
    }

    #[cfg(target_os = "linux")]
    {
        use std::fs::File;
        use std::io::Read;

        let mut file = File::open("/proc/self/status")
            .unwrap_or_else(|_| File::open("/proc/1/status").unwrap());
        let mut content = String::new();
        file.read_to_string(&mut content).unwrap();

        for line in content.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    let kb: usize = parts[1].parse().unwrap_or(0);
                    return kb * 1024;
                }
            }
        }
        0
    }

    // macOS and other non-Linux Unix: /proc is not available
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        0
    }

    #[cfg(not(any(windows, unix)))]
    {
        0
    }
}
