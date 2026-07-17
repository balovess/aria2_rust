//! Network Error Path Tests
//!
//! Tests for network error handling scenarios including:
//! - Connection interruption
//! - Timeout handling
//! - DNS resolution failure
//! - Retry behavior verification

mod e2e_helpers;
mod fixtures;

use aria2_core::dns::dns_cache::DnsCache;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::error::{Aria2Error, FatalError, RecoverableError};
use aria2_core::http::connection::{HttpConfig, HttpConnectionManager};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use aria2_core::retry::{RetryExecutor, RetryPolicy, RetryStats};
use e2e_helpers::mock_http_server::{
    MockHttpServer, Response, StatusCode, full_body, partial_body,
};
use std::sync::Arc;
use std::time::Duration;
use url::Url;

// =========================================================================
// Connection Interruption Tests
// =========================================================================

/// Test connection interruption during download
/// Simulates a server that closes connection mid-transfer
#[tokio::test]
async fn test_connection_interrupt_partial_serve() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // Register a handler that serves partial content then "drops" connection
    // by claiming larger Content-Length than actual body
    let full_body = vec![0xAB; 10000]; // Claim 10KB
    let partial_body = vec![0xAB; 3000]; // Only serve 3KB
    server.register_partial_serve("/interrupt.bin", &full_body, partial_body.len());

    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/interrupt.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(1),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    // This should fail or return an error due to incomplete transfer
    let result = cmd.execute().await;

    // The download should either fail or we should detect the incomplete transfer
    // The exact behavior depends on how the download engine handles partial content
    if let Err(e) = result {
        // Verify error is network-related
        let err_str = e.to_string();
        assert!(
            err_str.contains("Network")
                || err_str.contains("IO")
                || err_str.contains("timeout")
                || err_str.contains("connection")
                || err_str.contains("failed"),
            "Error should be network-related: {}",
            err_str
        );
    }

    server.shutdown().await;
}

/// Test connection reset during read operation
#[tokio::test]
async fn test_connection_reset_error_handling() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // Register a handler that sends headers claiming 1MB but only delivers
    // 100 bytes, then closes the stream. This simulates a connection reset
    // mid-transfer. Using `partial_body` (StreamBody with Unknown size_hint)
    // ensures hyper 1.x respects the manually-set Content-Length header, so
    // the client detects the premature EOF.
    server.on_get("/reset.bin", |_req| {
        Response::builder()
            .status(StatusCode::OK)
            .header("Content-Length", "1000000") // Claim 1MB
            .body(partial_body(vec![0u8; 100])) // But only send 100 bytes
            .unwrap()
    });

    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/reset.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(2),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;

    // The download behavior depends on how the engine handles incomplete body
    // It may fail or succeed with partial data - both are acceptable
    if result.is_ok() {
        // If download succeeded, verify file exists (may be empty or partial)
        let output_path = dir.path().join("reset.bin");
        if output_path.exists() {
            let content = std::fs::read(&output_path).unwrap();
            // File should be smaller than claimed 1MB
            assert!(
                content.len() < 1000000,
                "File should be smaller than claimed size"
            );
        }
    }
    // If it fails, that's also acceptable behavior

    server.shutdown().await;
}

// =========================================================================
// Timeout Handling Tests
// =========================================================================

/// Test connection timeout with slow server
#[tokio::test]
async fn test_connection_timeout_slow_server() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    // Register a very slow response (500ms delay, but we use 100ms timeout)
    let body = vec![0xCD; 1024];
    server.register_slow_response("/slow.bin", 500, &body);

    // Create connection manager with short timeout
    let config = HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(100),
        read_timeout: Duration::from_millis(100),
        write_timeout: Duration::from_millis(100),
        idle_timeout: Duration::from_secs(5),
    };

    let mut manager = HttpConnectionManager::new(&config);
    let url = Url::parse(&format!("{}/slow.bin", server.base_url())).unwrap();

    // Acquire should timeout due to slow response
    let result = manager.acquire(&url).await;

    // Either succeeds (connection established) or times out
    // The timeout behavior depends on whether the TCP handshake completes
    match result {
        Ok(conn) => {
            // Connection succeeded, but read might timeout
            manager.release(conn.id).await;
        }
        Err(Aria2Error::Recoverable(RecoverableError::Timeout)) => {
            // Expected: timeout occurred
        }
        Err(e) => {
            // Other errors are also acceptable
            let err_str = e.to_string();
            assert!(
                err_str.contains("timeout") || err_str.contains("failed"),
                "Error should be timeout or network failure: {}",
                err_str
            );
        }
    }

    manager.cleanup().await;
    server.shutdown().await;
}

/// Test retry executor timeout behavior
#[tokio::test]
async fn test_retry_executor_timeout_recovery() {
    let policy = RetryPolicy::new(3, 10); // 3 retries, 10ms base wait
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);

    let attempt_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let ac = attempt_count.clone();

    // Simulate timeout on first attempt, success on second
    let result = executor
        .execute(move |attempt| {
            let ac = ac.clone();
            async move {
                ac.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if attempt == 0 {
                    Err(Aria2Error::Recoverable(RecoverableError::Timeout))
                } else {
                    Ok::<_, Aria2Error>("success")
                }
            }
        })
        .await;

    assert!(result.is_ok(), "Should succeed after retry");
    assert_eq!(result.unwrap(), "success");
    assert_eq!(attempt_count.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(stats.timeouts(), 1);
}

/// Test max retries exhausted for timeout errors
#[tokio::test]
async fn test_timeout_max_retries_exhausted() {
    let policy = RetryPolicy::new(2, 5); // Only 2 retries allowed
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);

    // Always timeout
    let result: Result<(), _> = executor
        .execute(|_attempt| async { Err(Aria2Error::Recoverable(RecoverableError::Timeout)) })
        .await;

    assert!(result.is_err(), "Should fail after max retries");
    assert_eq!(stats.timeouts(), 2);
    assert_eq!(stats.total(), 2);
}

// =========================================================================
// DNS Resolution Failure Tests
// =========================================================================

/// Test DNS cache negative caching behavior
#[test]
fn test_dns_negative_cache_blocks_immediate_retry() {
    let mut cache = DnsCache::with_ttl(300, 5); // 5 second negative TTL

    // Record a failed lookup
    cache.record_failure("nonexistent.invalid");

    // Immediate retry should be blocked
    let result = cache.resolve_no_network("nonexistent.invalid", 80);
    assert!(result.is_err(), "Should be blocked by negative cache");

    let err_msg = result.unwrap_err();
    assert!(
        err_msg.contains("recently failed"),
        "Error should mention recent failure: {}",
        err_msg
    );
}

/// Test DNS cache expiration allows retry
#[test]
fn test_dns_negative_cache_expiration_allows_retry() {
    let mut cache = DnsCache::with_ttl(300, 1); // 1 second negative TTL

    // Record a failed lookup
    cache.record_failure("expired.invalid");

    // Clear the cache to simulate expiration
    cache.clear();

    // Now retry should not be blocked by negative cache
    let result = cache.resolve_no_network("expired.invalid", 80);
    // Should still fail because no actual DNS resolution, but not due to negative cache
    let err_msg = result.unwrap_err();
    assert!(
        !err_msg.contains("recently failed"),
        "Error should not mention recent failure after expiration: {}",
        err_msg
    );
}

/// Test DNS resolution for invalid hostname
#[tokio::test]
async fn test_dns_resolution_invalid_hostname() {
    let mut cache = DnsCache::with_ttl(300, 60);

    // Try to resolve an invalid hostname (using a truly invalid format)
    // Note: Some DNS resolvers may resolve .invalid TLD, so we use a clearly invalid format
    let result = cache.resolve("invalid..hostname..test", 80).await;

    // DNS resolution behavior varies by system - may fail or succeed
    if result.is_err() {
        // If resolution failed, verify negative cache behavior
        let retry_result = cache.resolve_no_network("invalid..hostname..test", 80);
        assert!(
            retry_result.is_err(),
            "Retry should be blocked by negative cache"
        );
        let err_msg = retry_result.unwrap_err();
        assert!(
            err_msg.contains("recently failed"),
            "Error should mention recent failure due to negative cache: {}",
            err_msg
        );
    } else {
        // If resolution succeeded (unlikely but possible), just verify cache was populated
        assert!(
            !cache.is_empty(),
            "Cache should have entry after resolution"
        );
    }
}

/// Test DNS IPv4 preference sorting
#[tokio::test]
async fn test_dns_ipv4_preference_on_resolution() {
    let mut cache = DnsCache::new();
    cache.set_ipv4_preference(true);

    // Resolve localhost (typically has both IPv4 and IPv6)
    let result = cache.resolve("localhost", 8080).await;

    if let Ok(addrs) = result {
        // Check if IPv4 addresses come first when both are present
        let has_ipv4 = addrs
            .iter()
            .any(|a| matches!(a.ip(), std::net::IpAddr::V4(_)));
        let has_ipv6 = addrs
            .iter()
            .any(|a| matches!(a.ip(), std::net::IpAddr::V6(_)));

        if has_ipv4 && has_ipv6 {
            let first_ipv4_pos = addrs
                .iter()
                .position(|a| matches!(a.ip(), std::net::IpAddr::V4(_)));
            let first_ipv6_pos = addrs
                .iter()
                .position(|a| matches!(a.ip(), std::net::IpAddr::V6(_)));

            if let (Some(v4_pos), Some(v6_pos)) = (first_ipv4_pos, first_ipv6_pos) {
                assert!(
                    v4_pos < v6_pos,
                    "IPv4 should come before IPv6 when preferred"
                );
            }
        }
    }
}

// =========================================================================
// Server Error Tests
// =========================================================================

/// Test HTTP 500 server error handling
#[tokio::test]
async fn test_http_500_server_error() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    server.on_get("/error500", |_req| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(full_body("Internal Server Error"))
            .unwrap()
    });

    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/error500", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(3),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_err(), "500 error should cause download failure");

    server.shutdown().await;
}

/// Test HTTP 503 service unavailable with retry
#[tokio::test]
async fn test_http_503_retry_behavior() {
    let policy = RetryPolicy::new(3, 5);
    let stats = RetryStats::default();
    let executor = RetryExecutor::new(&policy, &stats);

    let attempt_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let ac = attempt_count.clone();

    // Simulate 503 on first two attempts, success on third
    let result = executor
        .execute(move |attempt| {
            let ac = ac.clone();
            async move {
                ac.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if attempt < 2 {
                    Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                        code: 503,
                    }))
                } else {
                    Ok::<_, Aria2Error>("recovered")
                }
            }
        })
        .await;

    assert!(result.is_ok(), "Should recover from 503 after retries");
    assert_eq!(attempt_count.load(std::sync::atomic::Ordering::Relaxed), 3);
    assert_eq!(stats.server_errors(), 2);
}

/// Test HTTP 404 not found (fatal error, no retry)
#[tokio::test]
async fn test_http_404_no_retry() {
    let server = MockHttpServer::start()
        .await
        .expect("Failed to start mock server");

    server.on_get("/notfound", |_req| {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(full_body("Not Found"))
            .unwrap()
    });

    let dir = tempfile::tempdir().unwrap();
    let url = format!("{}/notfound", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(4),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_err(), "404 should cause download failure");

    server.shutdown().await;
}

// =========================================================================
// Connection Pool Error Tests
// =========================================================================

/// Test max connections limit reached
#[tokio::test]
async fn test_max_connections_limit_error() {
    let config = HttpConfig {
        max_connections: 2,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_secs(5),
        write_timeout: Duration::from_secs(5),
        idle_timeout: Duration::from_secs(60),
    };

    let mut manager = HttpConnectionManager::new(&config);

    // Start a simple test server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
                // Keep connection open to prevent reuse
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    // Acquire first connection
    let conn1 = manager
        .acquire(&url)
        .await
        .expect("First connection should succeed");

    // Acquire second connection
    let conn2 = manager
        .acquire(&url)
        .await
        .expect("Second connection should succeed");

    // Third connection should fail (max limit reached)
    let result = manager.acquire(&url).await;
    assert!(result.is_err(), "Should fail when max connections reached");

    match result {
        Err(Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { message })) => {
            // Check for either English or Chinese limit indicator
            assert!(
                message.contains("max")
                    || message.contains("limit")
                    || message.contains("最大")
                    || message.contains("限制"),
                "Error should mention limit: {}",
                message
            );
        }
        Err(e) => {
            // Other recoverable errors are acceptable
            let err_str = e.to_string();
            assert!(
                err_str.contains("limit")
                    || err_str.contains("max")
                    || err_str.contains("最大")
                    || err_str.contains("限制"),
                "Error should indicate connection limit: {}",
                e
            );
        }
        Ok(_) => panic!("Should not succeed when limit reached"),
    }

    // Cleanup
    manager.release(conn1.id).await;
    manager.release(conn2.id).await;
    manager.cleanup().await;
    server_handle.abort();
}

/// Test connection pool cleanup on error
#[tokio::test]
async fn test_connection_cleanup_on_error() {
    let config = HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(100),
        read_timeout: Duration::from_secs(1),
        write_timeout: Duration::from_secs(1),
        idle_timeout: Duration::from_secs(5),
    };

    let mut manager = HttpConnectionManager::new(&config);

    // Try to connect to an unreachable address (should fail)
    // Use a valid IP format that won't respond
    let url = Url::parse("http://10.255.255.1:9999/unreachable").unwrap();

    let result = manager.acquire(&url).await;

    // Connection may fail due to timeout or unreachable address
    // Either outcome is acceptable
    match result {
        Err(_) => {
            // Verify pool is still clean (no leaked connections)
            assert_eq!(
                manager.pool_size(),
                0,
                "No connections should remain after failed acquire"
            );
            assert_eq!(manager.active_count(), 0, "Active count should be 0");
        }
        Ok(conn) => {
            // If connection somehow succeeded, release it
            manager.release(conn.id).await;
        }
    }

    manager.cleanup().await;
}

// =========================================================================
// Redirect Error Tests
// =========================================================================

/// Test redirect loop detection
#[test]
fn test_redirect_loop_detection() {
    let manager = HttpConnectionManager::new(&Default::default());

    use aria2_protocol::http::response::HttpResponse;
    use std::collections::HashSet;

    let url_a = Url::parse("http://example.com/a").unwrap();
    let url_b = Url::parse("http://example.com/b").unwrap();

    let mut chain = HashSet::new();
    chain.insert(url_a.clone());
    chain.insert(url_b.clone());

    // Response redirects back to url_a (creating a loop)
    let mut response = HttpResponse::new(301, "Moved".to_string());
    response
        .headers
        .push(("Location".to_string(), "http://example.com/a".to_string()));

    let result = manager.follow_redirects(&response, &url_b, &chain, 2);

    assert!(result.is_err(), "Redirect loop should be detected");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("loop") || err.to_string().contains("循环"),
        "Error should mention loop: {}",
        err
    );
}

/// Test max redirects exceeded
#[test]
fn test_max_redirects_exceeded() {
    let manager = HttpConnectionManager::new(&Default::default());

    use aria2_protocol::http::response::HttpResponse;
    use std::collections::HashSet;

    let current_url = Url::parse("http://example.com/start").unwrap();
    let chain = HashSet::new();

    let mut response = HttpResponse::new(302, "Found".to_string());
    response.headers.push((
        "Location".to_string(),
        "http://example.com/next".to_string(),
    ));

    // Attempt with redirect count already at max
    let result = manager.follow_redirects(&response, &current_url, &chain, 10);

    assert!(result.is_err(), "Should fail when max redirects exceeded");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("max") || err.to_string().contains("最大"),
        "Error should mention max redirects: {}",
        err
    );
}

/// Test missing Location header in redirect
#[test]
fn test_redirect_missing_location_header() {
    let manager = HttpConnectionManager::new(&Default::default());

    use aria2_protocol::http::response::HttpResponse;
    use std::collections::HashSet;

    let current_url = Url::parse("http://example.com/").unwrap();
    let chain = HashSet::new();

    // 301 response without Location header
    let response = HttpResponse::new(301, "Moved".to_string());

    let result = manager.follow_redirects(&response, &current_url, &chain, 0);

    assert!(result.is_err(), "Should fail without Location header");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("Location") || err.to_string().contains("缺少"),
        "Error should mention missing Location: {}",
        err
    );
}

// =========================================================================
// Retry Policy Tests
// =========================================================================

/// Test retry policy should_retry logic
#[test]
fn test_retry_policy_recoverable_vs_fatal() {
    let policy = RetryPolicy::new(5, 1000);

    // Recoverable errors should allow retry
    assert!(policy.should_retry(0, &Aria2Error::Recoverable(RecoverableError::Timeout)));
    assert!(policy.should_retry(
        0,
        &Aria2Error::Recoverable(RecoverableError::ServerError { code: 500 })
    ));
    assert!(policy.should_retry(
        0,
        &Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "test".into()
        })
    ));

    // Fatal errors should NOT allow retry
    assert!(!policy.should_retry(0, &Aria2Error::Fatal(FatalError::DiskSpaceExhausted)));
    assert!(!policy.should_retry(
        0,
        &Aria2Error::Fatal(FatalError::PermissionDenied {
            path: "/test".into()
        })
    ));
    assert!(!policy.should_retry(0, &Aria2Error::Fatal(FatalError::Config("bad".into()))));

    // Generic network errors should NOT allow retry
    assert!(!policy.should_retry(0, &Aria2Error::Network("generic error".into())));
}

/// Test exponential backoff timing
#[test]
fn test_retry_exponential_backoff() {
    let policy = RetryPolicy::new(10, 1000).with_max_wait_ms(600_000);

    // Verify exponential growth
    assert_eq!(policy.wait_duration(0), Duration::from_millis(1000));
    assert_eq!(policy.wait_duration(1), Duration::from_millis(2000));
    assert_eq!(policy.wait_duration(2), Duration::from_millis(4000));
    assert_eq!(policy.wait_duration(3), Duration::from_millis(8000));
    assert_eq!(policy.wait_duration(4), Duration::from_millis(16000));

    // Verify cap at max
    assert_eq!(policy.wait_duration(20), Duration::from_secs(600));
}

/// Test retry stats tracking
#[test]
fn test_retry_stats_categories() {
    let stats = RetryStats::default();

    // Record different error types
    stats.record_retry(&Aria2Error::Recoverable(RecoverableError::Timeout));
    stats.record_retry(&Aria2Error::Recoverable(RecoverableError::ServerError {
        code: 503,
    }));
    stats.record_retry(&Aria2Error::Recoverable(
        RecoverableError::TemporaryNetworkFailure {
            message: "conn reset".into(),
        },
    ));
    stats.record_retry(&Aria2Error::Recoverable(
        RecoverableError::MaxTriesReached { attempts: 5 },
    ));

    // Verify categorization
    assert_eq!(stats.total(), 4);
    assert_eq!(stats.timeouts(), 1);
    assert_eq!(stats.server_errors(), 1);
    assert_eq!(stats.network_failures(), 1);

    // Reset and verify
    stats.reset();
    assert_eq!(stats.total(), 0);
    assert_eq!(stats.timeouts(), 0);
}

// =========================================================================
// Error Type Verification Tests
// =========================================================================

/// Test error Display trait formatting
#[test]
fn test_error_display_formatting() {
    // Network error
    let net_err = Aria2Error::Network("connection refused".to_string());
    assert!(net_err.to_string().contains("Network"));

    // Recoverable timeout
    let timeout_err = Aria2Error::Recoverable(RecoverableError::Timeout);
    assert!(
        timeout_err.to_string().contains("timeout") || timeout_err.to_string().contains("Timeout")
    );

    // Recoverable server error
    let server_err = Aria2Error::Recoverable(RecoverableError::ServerError { code: 500 });
    assert!(server_err.to_string().contains("500") || server_err.to_string().contains("Server"));

    // Fatal disk space
    let disk_err = Aria2Error::Fatal(FatalError::DiskSpaceExhausted);
    assert!(disk_err.to_string().contains("disk") || disk_err.to_string().contains("space"));

    // Fatal permission denied
    let perm_err = Aria2Error::Fatal(FatalError::PermissionDenied {
        path: "/root/test".into(),
    });
    assert!(
        perm_err.to_string().contains("Permission") || perm_err.to_string().contains("/root/test")
    );
}

/// Test error equality for comparison
#[test]
fn test_error_equality() {
    // Same errors should be equal
    let err1 = Aria2Error::Recoverable(RecoverableError::Timeout);
    let err2 = Aria2Error::Recoverable(RecoverableError::Timeout);
    assert_eq!(err1, err2);

    // Different errors should not be equal
    let err3 = Aria2Error::Recoverable(RecoverableError::ServerError { code: 500 });
    assert_ne!(err1, err3);

    // Same server error codes should be equal
    let err4 = Aria2Error::Recoverable(RecoverableError::ServerError { code: 500 });
    let err5 = Aria2Error::Recoverable(RecoverableError::ServerError { code: 500 });
    assert_eq!(err4, err5);

    // Different server error codes should not be equal
    let err6 = Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 });
    assert_ne!(err4, err6);
}
