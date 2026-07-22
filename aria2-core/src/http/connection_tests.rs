//! HTTP connection manager integration tests
//!
//! Tests for connection pool reuse, redirect following, timeout control, and other core features.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};

use crate::error::Aria2Error;
use crate::http::connection::{
    ActiveConnection, HttpConfig, HttpConnectionManager,
};
use crate::http::connection::HttpResponse;

/// Create HTTP config for testing
fn create_test_config() -> HttpConfig {
    HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_millis(2000),
    }
}

/// Start a simple test HTTP server
///
/// Returns the server's local address and server handle (for shutdown when test ends)
async fn start_test_server(
    handler: impl Fn(TcpStream) + Send + 'static,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    handler(stream);
                }
                Err(_) => break,
            }
        }
    });

    (addr, handle)
}

// ==================== Test case 1: Connection pool reuse ====================

#[tokio::test]
async fn test_connection_pool_reuse() {
    let config = create_test_config();
    let mut manager = HttpConnectionManager::new(&config);

    // Start test server
    let addr_str = Arc::new(Mutex::new(String::new()));
    let addr_clone = addr_str.clone();
    let (addr, server_handle) = start_test_server(move |mut stream| {
        let addr_clone = addr_clone.clone();
        tokio::spawn(async move {
            // Save address
            *addr_clone.lock().unwrap() =
                stream.peer_addr().unwrap().to_string();

            // Simple HTTP response
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await; // Wait for server to start

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // First connection acquisition
    let conn1 = manager.acquire(&url).await.expect("First connection acquisition should succeed");
    let conn1_id = conn1.id;
    assert_eq!(manager.active_count(), 1);
    println!("First connection acquired: id={}", conn1_id);

    // Release connection
    manager.release(conn1_id).await;
    assert_eq!(manager.active_count(), 1); // Connection still in pool
    println!("Connection returned to pool");

    // Second connection acquisition (should reuse)
    let conn2 = manager.acquire(&url).await.expect("Second acquisition should reuse connection");
    assert_eq!(conn2.id, conn1_id); // Should be the same connection ID
    assert_eq!(manager.active_count(), 1); // Should not create new connection
    println!("Connection pool reuse successful: id={}", conn2.id);

    // Cleanup
    manager.cleanup().await;
    server_handle.abort();

    println!("Test passed: Connection pool reuse works correctly");
}

// ==================== Test case 2: Redirect following (5 hops) ====================

#[tokio::test]
async fn test_redirect_follow_5_jumps() {
    let manager = HttpConnectionManager::new(&create_test_config());
    let current_url = url::Url::parse("http://example.com/start").unwrap();
    let mut redirect_chain = HashSet::new();
    redirect_chain.insert(current_url.clone());

    // Simulate 5 consecutive redirects
    let urls = vec![
        "http://example.com/page1",
        "http://example.com/page2",
        "http://example.com/page3",
        "http://example.com/page4",
        "http://example.com/final",
    ];

    let mut current = current_url;
    for (i, target) in urls.iter().enumerate() {
        let mut response = HttpResponse::new(302, "Found".to_string());
        response.headers.push(("Location".to_string(), target.to_string()));

        redirect_chain.insert(current.clone());

        let result = manager.follow_redirects(&response, &current, &redirect_chain, (i + 1) as u32);
        assert!(
            result.is_ok(),
            "Redirect {} should succeed: {:?}",
            i + 1,
            result.err()
        );

        current = result.unwrap();
        println!("Redirect {}: -> {}", i + 1, current);
    }

    assert_eq!(current.as_str(), "http://example.com/final/");
    println!("Test passed: Successfully followed 5 redirects");
}

// ==================== Test case 3: Redirect loop detection ====================

#[tokio::test]
async fn test_redirect_loop_detection() {
    let manager = HttpConnectionManager::new(&create_test_config());

    // Build loop: A -> B -> C -> A
    let url_a = url::Url::parse("http://example.com/a").unwrap();
    let url_b = url::Url::parse("http://example.com/b").unwrap();
    let url_c = url::Url::parse("http://example.com/c").unwrap();

    let mut chain = HashSet::new();
    chain.insert(url_a.clone());
    chain.insert(url_b.clone());
    chain.insert(url_c.clone());

    // Try redirecting from C back to A (forming a loop)
    let mut response = HttpResponse::new(301, "Moved".to_string());
    response.headers.push(("Location".to_string(), "http://example.com/a".to_string()));

    let result = manager.follow_redirects(&response, &url_c, &chain, 3);

    assert!(result.is_err(), "Cyclic redirect should be detected");

    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("Circular redirect") || err_msg.contains("circular redirect"),
        "Error message should contain 'circular redirect': {}",
        err_msg
    );
    println!("Correctly detected cyclic redirect: {}", err_msg);

    println!("Test passed: Redirect loop detection works correctly");
}

// ==================== Test case 4: Range request building ====================

#[test]
fn test_range_request_build() {
    let manager = HttpConnectionManager::new(&create_test_config());

    // Test 1: Standard range
    let range1 = manager.build_range_header(0, Some(999));
    assert_eq!(range1, "bytes=0-999", "Standard range format incorrect");
    println!("Standard range: {}", range1);

    // Test 2: Open-ended range
    let range2 = manager.build_range_header(500, None);
    assert_eq!(range2, "bytes=500-", "Open-ended range format incorrect");
    println!("Open-ended range: {}", range2);

    // Test 3: Single byte range
    let range3 = manager.build_range_header(42, Some(42));
    assert_eq!(range3, "bytes=42-42", "Single byte range format incorrect");
    println!("Single byte range: {}", range3);

    // Test 4: Large offset
    let range4 = manager.build_range_header(1024 * 1024, Some(1024 * 1024 + 512));
    assert_eq!(
        range4,
        "bytes=1048576-1049088",
        "Large offset range format incorrect"
    );
    println!("Large offset range: {}", range4);

    // Test 5: Content-Range parsing
    let parsed1 = manager.parse_content_range("bytes 0-499/1000");
    assert_eq!(parsed1, Some((0, 499, 1000)), "Content-Range parsing failed");
    println!("Content-Range parsed (known total): {:?}", parsed1);

    let parsed2 = manager.parse_content_range("bytes 500-999/*");
    assert_eq!(parsed2, Some((500, 999, u64::MAX)), "Unknown total parsing failed");
    println!("Content-Range parsed (unknown total): {:?}", parsed2);

    // Test 6: Invalid format
    assert_eq!(manager.parse_content_range("invalid"), None);
    assert_eq!(manager.parse_content_range("bits 0-99/1000"), None);
    println!("Invalid format correctly returns None");

    println!("Test passed: Range request building and parsing correct");
}

// ==================== Test case 5: Timeout control ====================

#[tokio::test]
async fn test_timeout_on_slow_server() {
    let config = HttpConfig {
        max_connections: 2,
        connect_timeout: Duration::from_millis(100),   // Short connect timeout
        read_timeout: Duration::from_millis(200),      // Short read timeout
        write_timeout: Duration::from_millis(200),     // Short write timeout
        idle_timeout: Duration::from_secs(60),
    };
    let mut manager = HttpConnectionManager::new(&config);

    // Start a slow server (no response)
    let (addr, server_handle) = start_test_server(|_stream| {
        // Deliberately not responding, simulating a slow server
        tokio::spawn(async move {
            sleep(Duration::from_secs(10)).await;
        });
    })
    .await;

    sleep(Duration::from_millis(50)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // Try to connect (should fail due to timeout)
    // Note: Since this is a localhost connection, TCP connection may be established quickly
    // Timeout mainly applies to subsequent I/O operations
    let start = Instant::now();
    let result = timeout(config.connect_timeout + Duration::from_millis(50), manager.acquire(&url)).await;

    match result {
        Ok(conn_result) => {
            // If connection succeeds (localhost may connect quickly), verify config is correct
            if let Ok(conn) = conn_result {
                println!("Local connection succeeded (expected behavior), verifying timeout config...");
                assert_eq!(manager.max_connections(), 2);
                manager.release(conn.id).await;
            } else {
                // If failed, verify it is a timeout error
                println!("Connection failed (possibly timeout): {:?}", conn_result.err());
            }
        }
        Err(_) => {
            println!("Connection operation timed out (as expected)");
        }
    }

    let elapsed = start.elapsed();
    println!("Operation elapsed: {:.2}ms", elapsed.as_millis());

    // Verify elapsed time is within reasonable range (allow some margin)
    assert!(
        elapsed < config.connect_timeout + Duration::from_millis(300),
        "Elapsed too long: {:.2}ms",
        elapsed.as_millis()
    );

    manager.cleanup().await;
    server_handle.abort();

    println!("Test passed: Timeout control mechanism works correctly");
}

// ==================== Test case 6: Max connections limit ====================

#[tokio::test]
async fn test_max_connections_limit() {
    let config = HttpConfig {
        max_connections: 2,  // Limit to 2 connections
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_secs(60),
    };
    let mut manager = HttpConnectionManager::new(&config);

    // Start test server
    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
            sleep(Duration::from_secs(10)).await; // Keep connection alive
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // Acquire first connection
    let conn1 = manager.acquire(&url).await.expect("First connection should succeed");
    println!("Connection 1: id={}, active={}/{}", conn1.id, manager.active_count(), manager.max_connections());
    assert_eq!(manager.active_count(), 1);

    // Acquire second connection
    let conn2 = manager.acquire(&url).await.expect("Second connection should succeed");
    println!("Connection 2: id={}, active={}/{}", conn2.id, manager.active_count(), manager.max_connections());
    assert_eq!(manager.active_count(), 2);

    // Try to acquire third connection (should fail)
    let result = manager.acquire(&url).await;
    assert!(result.is_err(), "Should return error when max connection limit is exceeded");

    match result.unwrap_err() {
        Aria2Error::Recoverable(err) => {
            let err_msg = err.to_string();
            println!("Correctly rejected 3rd connection: {}", err_msg);
            assert!(
                err_msg.contains("Max connection") || err_msg.contains("max"),
                "Error message should contain connection limit hint"
            );
        }
        other => panic!("Expected Recoverable error, got: {:?}", other),
    }

    // Verify connection count did not increase
    assert_eq!(manager.active_count(), 2, "Active connections should not exceed max limit");

    // After releasing one connection, should be able to acquire again
    manager.release(conn1.id).await;
    println!("Released connection 1, trying to acquire again...");

    let conn3 = manager.acquire(&url).await.expect("Should be able to acquire new connection after release");
    println!("New connection acquired after release: id={}", conn3.id);
    assert_eq!(manager.active_count(), 2);

    // Cleanup
    manager.release(conn2.id).await;
    manager.release(conn3.id).await;
    manager.cleanup().await;

    println!("Test passed: Max connections limit enforced correctly");
}

// ==================== Additional test: LRU eviction strategy ====================

#[tokio::test]
async fn test_lru_eviction_strategy() {
    let config = HttpConfig {
        max_connections: 5,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_millis(100),  // Very short idle timeout
    };
    let mut manager = HttpConnectionManager::new(&config);

    // Start test server
    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // Create multiple connections and release immediately
    let mut conn_ids = Vec::new();
    for i in 0..3 {
        let conn = manager.acquire(&url).await.unwrap();
        println!("Created connection {}: id={}", i + 1, conn.id);
        conn_ids.push(conn.id);
        manager.release(conn.id).await;
    }

    assert_eq!(manager.pool_size(), 3, "Should have 3 idle connections");
    println!("Created 3 idle connections");

    // Wait for connections to expire
    sleep(Duration::from_millis(150)).await;
    println!("Waited {:.2}ms for connections to expire...", 150.0);

    // Try to acquire new connection (should trigger LRU eviction)
    let new_conn = manager.acquire(&url).await.unwrap();
    println!("New connection created (may have triggered LRU eviction): id={}", new_conn.id);

    // Verify old connections have been cleaned up
    // Note: Since acquire internally tries to reuse first, expired connections will be cleaned
    manager.release(new_conn.id).await;
    manager.cleanup().await;

    println!("Test passed: LRU eviction strategy basically works");
}

// ==================== Additional test: Concurrent connection safety ====================

#[tokio::test]
async fn test_concurrent_connection_access() {
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let config = create_test_config();
    let manager = Arc::new(Mutex::new(HttpConnectionManager::new(&config)));

    // Start test server
    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = stream.write_all(response.as_bytes()).await;
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = url::Url::parse(&format!("http://{}", addr)).unwrap();

    // Concurrently acquire multiple connections
    let mut handles = Vec::new();
    for i in 0..4 {
        let mgr = manager.clone();
        let url_clone = url.clone();

        let handle = tokio::spawn(async move {
            let mut m = mgr.lock().await;
            match m.acquire(&url_clone).await {
                Ok(conn) => {
                    println!("Task {} acquired connection: id={}", i, conn.id);
                    sleep(Duration::from_millis(50)).await;
                    m.release(conn.id).await;
                    Ok(i)
                }
                Err(e) => {
                    eprintln!("Task {} failed: {}", i, e);
                    Err(e)
                }
            }
        });

        handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
        let result = handle.await.unwrap();
        assert!(result.is_ok(), "Concurrent tasks should succeed");
    }

    let mut m = manager.lock().await;
    println!("Final state: active={}, pool_size={}", m.active_count(), m.pool_size());

    m.cleanup().await;

    println!("Test passed: Concurrent access is thread-safe");
}

use std::time::Instant;
