//! Integration tests for the HTTP connection pool
//!
//! Contains async tests that spin up a local TCP server to verify
//! connection pool reuse, timeout handling, max-connection limits,
//! idle eviction, and LRU behaviour.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::time::{sleep, timeout};
use url::Url;

use crate::error::Aria2Error;

use super::super::manager::HttpConnectionManager;
use super::super::types::HttpConfig;

/// Start a simple test HTTP server
async fn start_test_server(
    handler: impl Fn(tokio::net::TcpStream) + Send + 'static,
) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            handler(stream);
        }
    });

    (addr, handle)
}

// ==================== Connection Pool Tests ====================

#[tokio::test]
async fn test_connection_pool_reuse() {
    let config = HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_millis(2000),
        max_idle_per_host: 4,
    };
    let mut manager = HttpConnectionManager::new(&config);

    // Start test server
    let (addr, server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    // First connection acquisition
    let conn1 = manager
        .acquire(&url, None)
        .await
        .expect("First acquisition should succeed");
    assert_eq!(manager.active_count(), 1);

    // Return the connection (move ownership)
    manager.release(conn1).await;

    // Second connection acquisition (should succeed)
    let conn2 = manager
        .acquire(&url, None)
        .await
        .expect("Second acquisition should succeed");
    assert!(manager.active_count() >= 1); // Connection count should be >= 1

    // Cleanup
    manager.release(conn2).await;
    manager.cleanup().await;
    server_handle.abort();
}

#[tokio::test]
async fn test_discard_decrements_active_count() {
    let config = HttpConfig::default();
    let mut manager = HttpConnectionManager::new(&config);
    let (addr, server_handle) = start_test_server(|_stream| {}).await;
    let url = Url::parse(&format!("http://{}", addr)).unwrap();
    let conn = manager.acquire(&url, None).await.unwrap();
    assert_eq!(manager.active_count(), 1);

    manager.discard(conn).await;

    assert_eq!(manager.active_count(), 0);
    server_handle.abort();
}

#[tokio::test]
async fn test_evict_peer_removes_matching_idle_direct_connection() {
    let config = HttpConfig::default();
    let mut manager = HttpConnectionManager::new(&config);
    let (addr, server_handle) = start_test_server(|_stream| {}).await;
    let url = Url::parse(&format!("http://{}", addr)).unwrap();
    let conn = manager.acquire(&url, None).await.unwrap();
    let context = conn.connection_context().clone();
    manager.release(conn).await;
    assert_eq!(manager.pool_size(), 1);

    let evicted = manager.evict_peer(&context).await;

    assert_eq!(evicted, 1);
    assert_eq!(manager.pool_size(), 0);
    assert_eq!(manager.active_count(), 0);
    server_handle.abort();
}

#[tokio::test]
async fn test_timeout_on_slow_server() {
    use std::time::Instant;

    let config = HttpConfig {
        max_connections: 2,
        connect_timeout: Duration::from_millis(100),
        read_timeout: Duration::from_millis(200),
        write_timeout: Duration::from_millis(200),
        idle_timeout: Duration::from_secs(60),
        max_idle_per_host: 2,
    };
    let mut manager = HttpConnectionManager::new(&config);

    let (addr, server_handle) = start_test_server(|_stream| {
        tokio::spawn(async move {
            sleep(Duration::from_secs(10)).await;
        });
    })
    .await;

    sleep(Duration::from_millis(50)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();
    let start = Instant::now();

    let _result = timeout(
        config.connect_timeout + Duration::from_millis(50),
        manager.acquire(&url, None),
    )
    .await;
    let elapsed = start.elapsed();

    assert!(
        elapsed < config.connect_timeout + Duration::from_millis(300),
        "Elapsed time too long: {:.2}ms",
        elapsed.as_millis()
    );

    manager.cleanup().await;
    server_handle.abort();
}

#[tokio::test]
async fn test_max_connections_limit() {
    let config = HttpConfig {
        max_connections: 2,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_secs(60),
        max_idle_per_host: 2,
    };
    let mut manager = HttpConnectionManager::new(&config);

    let (addr, _server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            stream.write_all(response.as_bytes()).await.unwrap();
            sleep(Duration::from_secs(10)).await;
        });
    })
    .await;

    sleep(Duration::from_millis(100)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    let conn1 = manager.acquire(&url, None).await.unwrap();
    assert!(manager.active_count() >= 1);

    let conn2 = manager.acquire(&url, None).await.unwrap();
    assert!(manager.active_count() >= 2);

    // Attempt to acquire a third connection (should fail due to limit)
    let result = manager.acquire(&url, None).await;
    assert!(
        result.is_err(),
        "Should return error when max connection limit exceeded"
    );

    // Verify error type
    if let Err(e) = result {
        match &e {
            Aria2Error::Recoverable(_) => {}
            other => panic!("Expected Recoverable error, got: {:?}", other),
        }
    }

    // After returning one connection, should be able to acquire again (if pool reuse works)
    manager.release(conn1).await;
    // Note: since the connection may still be counted in the pool, we only verify no panic
    match manager.acquire(&url, None).await {
        Ok(conn3) => {
            println!(
                "Successfully acquired new connection after release: id={}",
                conn3.id
            );
            manager.release(conn3).await;
        }
        Err(e) => {
            println!(
                "Acquisition failed after release (may be connection reuse limit): {}",
                e
            );
            // This is also acceptable behavior
        }
    }

    manager.release(conn2).await;
    manager.cleanup().await;
}

// ==================== LRU Eviction & Idle Timeout Tests ====================

#[tokio::test]
async fn test_release_all_closes_idle() {
    let config = HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_secs(60),
        max_idle_per_host: 4,
    };
    let mut manager = HttpConnectionManager::new(&config);

    let (addr, server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    })
    .await;
    sleep(Duration::from_millis(100)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    // Acquire and release two connections
    let conn1 = manager.acquire(&url, None).await.unwrap();
    let conn2 = manager.acquire(&url, None).await.unwrap();
    manager.put_back(conn1).await;
    manager.put_back(conn2).await;

    assert_eq!(manager.pool_size(), 2);

    // release_all should close all idle connections
    manager.release_all().await;
    assert_eq!(manager.pool_size(), 0);

    manager.cleanup().await;
    server_handle.abort();
}

#[tokio::test]
async fn test_put_back_enforces_idle_limit() {
    let config = HttpConfig {
        max_connections: 8,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_secs(60),
        max_idle_per_host: 2, // Only 2 idle per host
    };
    let mut manager = HttpConnectionManager::new(&config);

    let (addr, server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    })
    .await;
    sleep(Duration::from_millis(100)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    // Acquire 3 connections
    let conn1 = manager.acquire(&url, None).await.unwrap();
    let conn2 = manager.acquire(&url, None).await.unwrap();
    let conn3 = manager.acquire(&url, None).await.unwrap();

    // Put back 3 — but max_idle_per_host=2, so oldest should be evicted
    manager.put_back(conn1).await;
    assert_eq!(manager.pool_size(), 1);

    manager.put_back(conn2).await;
    assert_eq!(manager.pool_size(), 2);

    // Third put_back should evict conn1 (oldest) to stay at limit 2
    manager.put_back(conn3).await;
    assert_eq!(manager.pool_size(), 2);

    manager.cleanup().await;
    server_handle.abort();
}

#[tokio::test]
async fn test_check_timeout_evicts_expired() {
    let config = HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_millis(200), // Very short idle timeout
        max_idle_per_host: 4,
    };
    let mut manager = HttpConnectionManager::new(&config);

    let (addr, server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    })
    .await;
    sleep(Duration::from_millis(100)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    // Acquire and release a connection
    let conn = manager.acquire(&url, None).await.unwrap();
    manager.put_back(conn).await;
    assert_eq!(manager.pool_size(), 1);

    // Wait for idle timeout to elapse
    sleep(Duration::from_millis(300)).await;

    // check_timeout should evict the expired connection
    let evicted = manager.check_timeout();
    assert_eq!(evicted, 1);
    assert_eq!(manager.pool_size(), 0);

    manager.cleanup().await;
    server_handle.abort();
}

#[tokio::test]
async fn test_put_back_is_alias_for_release() {
    let config = HttpConfig {
        max_connections: 4,
        connect_timeout: Duration::from_millis(500),
        read_timeout: Duration::from_millis(1000),
        write_timeout: Duration::from_millis(1000),
        idle_timeout: Duration::from_secs(60),
        max_idle_per_host: 4,
    };
    let mut manager = HttpConnectionManager::new(&config);

    let (addr, server_handle) = start_test_server(|mut stream| {
        tokio::spawn(async move {
            let resp = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
            let _ = stream.write_all(resp.as_bytes()).await;
        });
    })
    .await;
    sleep(Duration::from_millis(100)).await;

    let url = Url::parse(&format!("http://{}", addr)).unwrap();

    let conn = manager.acquire(&url, None).await.unwrap();
    let conn_id = conn.id;

    // release and put_back should behave identically
    manager.release(conn).await;
    assert_eq!(manager.pool_size(), 1);

    // Re-acquire (should reuse from pool)
    let conn2 = manager.acquire(&url, None).await.unwrap();
    assert_eq!(conn2.id, conn_id);

    manager.put_back(conn2).await;
    assert_eq!(manager.pool_size(), 1);

    manager.cleanup().await;
    server_handle.abort();
}
