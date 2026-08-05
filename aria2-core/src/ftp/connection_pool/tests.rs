use std::sync::Arc;
use std::time::Duration;

use super::*;

#[test]
fn test_connection_key_equality() {
    let key1 = ConnectionKey::new("example.com", 21, "user", "pass", "/");
    let key2 = ConnectionKey::new("example.com", 21, "user", "pass", "/");
    let key3 = ConnectionKey::new("example.com", 21, "user2", "pass", "/");
    let key4 = ConnectionKey::new("example.com", 21, "user", "pass", "/pub");

    assert_eq!(key1, key2);
    assert_ne!(key1, key3); // different username
    assert_ne!(key1, key4); // different base_working_dir
}

#[test]
fn test_connection_key_simple() {
    let key = ConnectionKey::new_simple("example.com", 21, "user", "pass");
    assert_eq!(key.base_working_dir, "/");
}

#[test]
fn test_pool_key_string() {
    let key1 = ConnectionKey::new("ftp.example.com", 21, "admin", "pass", "/");
    assert_eq!(key1.to_pool_key_string(), "admin@ftp.example.com(21)");

    let key2 = ConnectionKey::new("ftp.example.com", 21, "", "pass", "/");
    assert_eq!(key2.to_pool_key_string(), "ftp.example.com(21)");
}

#[test]
fn test_pool_config_default() {
    let config = PoolConfig::default();
    assert_eq!(config.max_connections, 16);
    assert_eq!(config.max_idle_time, Duration::from_secs(300));
    assert_eq!(config.max_connection_age, Duration::from_secs(1800));
}

#[tokio::test]
async fn test_pool_creation() {
    let pool = FtpConnectionPool::new(10);
    assert_eq!(pool.size().await, 0);
}

#[tokio::test]
async fn test_pool_stats_initial() {
    let pool = FtpConnectionPool::new(10);
    let stats = pool.stats().await;
    assert_eq!(stats.connections_created, 0);
    assert_eq!(stats.connections_reused, 0);
    assert_eq!(stats.connections_evicted, 0);
    assert_eq!(stats.current_size, 0);
}

#[tokio::test]
async fn test_pool_clear() {
    let pool = FtpConnectionPool::new(10);
    pool.clear().await;
    assert_eq!(pool.size().await, 0);
}

#[test]
fn test_pooled_connection_health() {
    let max_idle_time = Duration::from_secs(300);
    let idle_time = Duration::from_secs(10);
    assert!(idle_time < max_idle_time);

    let idle_time_long = Duration::from_secs(400);
    assert!(idle_time_long >= max_idle_time);
}

#[test]
fn test_lru_entry_creation() {
    let key = ConnectionKey::new("example.com", 21, "user", "pass", "/");
    let entry = LruEntry {
        key: key.clone(),
        last_access: std::time::Instant::now(),
    };

    assert_eq!(entry.key, key);
    assert!(entry.last_access.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn test_create_pool_returns_shared_arc() {
    let pool = create_pool(10);
    let pool2 = pool.clone();
    assert!(Arc::ptr_eq(&pool, &pool2));
}

#[tokio::test]
async fn test_custom_pool_is_different() {
    let pool1 = create_pool(10);
    let pool2 = create_custom_pool(PoolConfig::default());
    assert!(!Arc::ptr_eq(&pool1, &pool2));
}

#[test]
fn test_pool_stats_default() {
    let stats = PoolStats::default();
    assert_eq!(stats.connections_created, 0);
    assert_eq!(stats.connections_reused, 0);
    assert_eq!(stats.connections_evicted, 0);
    assert_eq!(stats.connection_failures, 0);
    assert_eq!(stats.current_size, 0);
    assert_eq!(stats.peak_size, 0);
}

#[test]
fn test_connection_key_hash() {
    use std::collections::HashSet;

    let mut set = HashSet::new();
    let key1 = ConnectionKey::new("example.com", 21, "user", "pass", "/");
    let key2 = ConnectionKey::new("example.com", 21, "user", "pass", "/");
    let key3 = ConnectionKey::new("other.com", 21, "user", "pass", "/");

    set.insert(key1.clone());
    assert!(set.contains(&key2)); // Same key
    assert!(!set.contains(&key3)); // Different key
}

#[tokio::test]
async fn test_try_get_returns_none_when_empty() {
    let pool = FtpConnectionPool::new(10);
    let result = pool.try_get("example.com", 21, "user", "pass", "/").await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_try_get_relaxed_returns_none_when_empty() {
    let pool = FtpConnectionPool::new(10);
    let result = pool
        .try_get_relaxed("example.com", 21, "user", "pass")
        .await;
    assert!(result.is_none());
}

#[tokio::test]
async fn test_cleanup_stale_count_on_empty_pool() {
    let pool = FtpConnectionPool::new(2);
    assert_eq!(pool.cleanup_stale_count().await, 0);
}
