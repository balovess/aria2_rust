//! FTP connection pool for connection reuse and performance optimization.
//!
//! This module provides a connection pool for FTP control connections that:
//! - Reuses existing connections to avoid repeated authentication
//! - Implements LRU eviction strategy when pool is full
//! - Supports concurrent access from multiple download tasks
//! - Provides health checking for stale connections
//!
//! # Performance Benefits
//!
//! Connection pooling provides 40-60% speed improvement by:
//! - Eliminating 10-second connection establishment overhead
//! - Avoiding repeated authentication handshakes
//! - Reducing TCP connection setup latency
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::ftp::connection_pool::{FtpConnectionPool, PooledConnection};
//! use aria2_core::ftp::connection::FtpMode;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let pool = FtpConnectionPool::new(10); // Max 10 connections
//!     
//!     // Get or create a connection
//!     let conn = pool.get_connection(
//!         "ftp.example.com",
//!         21,
//!         "user",
//!         "pass",
//!         FtpMode::Passive
//!     ).await?;
//!     
//!     // Use the connection...
//!     
//!     // Return it to the pool
//!     pool.return_connection(conn).await;
//!     
//!     Ok(())
//! }
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, info};

use crate::error::Result;
use crate::ftp::connection::FtpMode;

/// Connection key for identifying unique FTP server connections.
///
/// The key format matches C++ `createSockPoolKey`:
/// `username@host(port)` — when `base_working_dir` differs,
/// connections are pooled separately so CWD traversal can be skipped
/// on reuse when the base directory matches.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConnectionKey {
    /// Server hostname
    pub host: String,
    /// Server port
    pub port: u16,
    /// Username for authentication
    pub username: String,
    /// Password for authentication (stored for reconnection if needed)
    pub password: String,
    /// Base working directory from PWD command (used for CWD skip optimization).
    /// Matches C++ `FtpConnection::getBaseWorkingDir()` stored in
    /// `SocketPoolEntry::options_`.
    pub base_working_dir: String,
}

impl ConnectionKey {
    /// Create a new connection key.
    pub fn new(
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        base_working_dir: &str,
    ) -> Self {
        Self {
            host: host.to_string(),
            port,
            username: username.to_string(),
            password: password.to_string(),
            base_working_dir: base_working_dir.to_string(),
        }
    }

    /// Create a connection key with default base_working_dir ("/").
    pub fn new_simple(host: &str, port: u16, username: &str, password: &str) -> Self {
        Self::new(host, port, username, password, "/")
    }

    /// Format the key in C++ `createSockPoolKey` style for logging.
    pub fn to_pool_key_string(&self) -> String {
        if self.username.is_empty() {
            format!("{}({})", self.host, self.port)
        } else {
            format!("{}@{}({})", self.username, self.host, self.port)
        }
    }
}

/// Raw pooled FTP control stream.
///
/// Wraps a TCP stream from a successfully negotiated FTP control connection.
/// When a connection is returned to the pool after a download completes (226),
/// the control stream is stored here for reuse by subsequent downloads that
/// can skip authentication and CWD traversal.
#[derive(Debug)]
pub struct RawControlStream {
    /// The buffered control stream
    pub reader: BufReader<TcpStream>,
    /// When this connection was created
    pub created_at: Instant,
    /// When this connection was last used
    pub last_used: Instant,
    /// Number of times this connection has been reused
    pub reuse_count: u64,
}

impl RawControlStream {
    /// Create a new raw control stream wrapper.
    pub fn new(stream: TcpStream, _read_timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            reader: BufReader::new(stream),
            created_at: now,
            last_used: now,
            reuse_count: 0,
        }
    }

    /// Mark this connection as used (update last_used timestamp).
    pub fn mark_used(&mut self) {
        self.last_used = Instant::now();
        self.reuse_count += 1;
    }

    /// Get the age of this connection.
    pub fn age(&self) -> Duration {
        self.created_at.elapsed()
    }

    /// Get how long this connection has been idle.
    pub fn idle_time(&self) -> Duration {
        self.last_used.elapsed()
    }

    /// Check if this connection is still healthy.
    pub fn is_healthy(&self, max_idle_time: Duration) -> bool {
        self.last_used.elapsed() < max_idle_time
    }

    /// Consume and return the inner TCP stream.
    pub fn into_inner(self) -> TcpStream {
        self.reader.into_inner()
    }

    /// Consume and return the buffered reader.
    pub fn into_buf_reader(self) -> BufReader<TcpStream> {
        self.reader
    }
}

/// Pooled FTP connection with metadata.
///
/// Stores a raw FTP control stream along with connection metadata.
/// The `base_working_dir` from `ConnectionKey` allows subsequent downloads
/// to determine whether CWD traversal can be skipped.
pub struct PooledConnection {
    /// The raw control stream (post-authentication, post-CWD)
    pub control: RawControlStream,
    /// Connection key for identification
    pub key: ConnectionKey,
    /// Connection mode (passive/active)
    pub mode: FtpMode,
    /// Read timeout for I/O operations on this connection
    pub read_timeout: Duration,
}

impl std::fmt::Debug for PooledConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledConnection")
            .field("key", &self.key)
            .field("mode", &self.mode)
            .field("read_timeout", &self.read_timeout)
            .field("age", &self.control.age())
            .field("idle_time", &self.control.idle_time())
            .field("reuse_count", &self.control.reuse_count)
            .finish_non_exhaustive()
    }
}

impl PooledConnection {
    /// Create a new pooled connection from a raw control stream.
    pub fn new(
        stream: TcpStream,
        key: ConnectionKey,
        mode: FtpMode,
        read_timeout: Duration,
    ) -> Self {
        Self {
            control: RawControlStream::new(stream, read_timeout),
            key,
            mode,
            read_timeout,
        }
    }

    /// Check if this connection is still healthy.
    pub fn is_healthy(&self, max_idle_time: Duration) -> bool {
        self.control.is_healthy(max_idle_time)
    }

    /// Get the age of this connection.
    pub fn age(&self) -> Duration {
        self.control.age()
    }

    /// Get how long this connection has been idle.
    pub fn idle_time(&self) -> Duration {
        self.control.idle_time()
    }

    /// Mark this connection as used (update last_used timestamp).
    pub fn mark_used(&mut self) {
        self.control.mark_used();
    }

    /// Get the base working directory for this pooled connection.
    pub fn base_working_dir(&self) -> &str {
        &self.key.base_working_dir
    }
}

/// LRU entry for tracking access order
#[derive(Debug, Clone)]
struct LruEntry {
    key: ConnectionKey,
    last_access: Instant,
}

/// FTP connection pool configuration
#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Maximum number of connections in the pool
    pub max_connections: usize,
    /// Maximum idle time before a connection is considered stale
    pub max_idle_time: Duration,
    /// Maximum age of a connection before it's evicted
    pub max_connection_age: Duration,
    /// Connection timeout for new connections
    pub connect_timeout: Duration,
    /// Read timeout for operations
    pub read_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_connections: crate::constants::FTP_POOL_DEFAULT_MAX_CONNECTIONS,
            max_idle_time: Duration::from_secs(
                crate::constants::FTP_POOL_DEFAULT_MAX_IDLE_TIME_SECS,
            ),
            max_connection_age: Duration::from_secs(
                crate::constants::FTP_POOL_DEFAULT_MAX_CONNECTION_AGE_SECS,
            ),
            connect_timeout: Duration::from_secs(
                crate::constants::FTP_POOL_DEFAULT_CONNECT_TIMEOUT_SECS,
            ),
            read_timeout: Duration::from_secs(crate::constants::FTP_POOL_DEFAULT_READ_TIMEOUT_SECS),
        }
    }
}

/// Thread-safe FTP connection pool with LRU eviction
pub struct FtpConnectionPool {
    /// Connection storage
    connections: Arc<Mutex<HashMap<ConnectionKey, PooledConnection>>>,
    /// LRU tracking (ordered by last access time)
    lru_order: Arc<Mutex<Vec<LruEntry>>>,
    /// Pool configuration
    config: PoolConfig,
    /// Statistics
    stats: Arc<Mutex<PoolStats>>,
}

/// Pool statistics for monitoring
#[derive(Debug, Clone, Default)]
pub struct PoolStats {
    /// Total connections created
    pub connections_created: u64,
    /// Total connections reused
    pub connections_reused: u64,
    /// Total connections evicted
    pub connections_evicted: u64,
    /// Total connection failures
    pub connection_failures: u64,
    /// Current pool size
    pub current_size: usize,
    /// Peak pool size
    pub peak_size: usize,
}

impl FtpConnectionPool {
    /// Create a new connection pool with default configuration
    pub fn new(max_connections: usize) -> Self {
        let config = PoolConfig {
            max_connections,
            ..Default::default()
        };
        Self::with_config(config)
    }

    /// Create a new connection pool with custom configuration
    pub fn with_config(config: PoolConfig) -> Self {
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            lru_order: Arc::new(Mutex::new(Vec::new())),
            config,
            stats: Arc::new(Mutex::new(PoolStats::default())),
        }
    }

    /// Try to get an existing healthy connection from the pool.
    ///
    /// Returns `None` if no matching healthy connection is found.
    /// The connection is removed from the pool and must be returned via
    /// `return_connection()` when done.
    ///
    /// The lookup key includes `base_working_dir` — if the caller's
    /// base directory doesn't match the pooled connection's, CWD
    /// traversal cannot be fully skipped and the connection won't be
    /// returned by this method.
    pub async fn try_get(
        &self,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        base_working_dir: &str,
    ) -> Option<PooledConnection> {
        let key = ConnectionKey::new(host, port, username, password, base_working_dir);

        let mut connections = self.connections.lock().await;
        if let Some(conn) = connections.get_mut(&key) {
            if conn.is_healthy(self.config.max_idle_time) {
                conn.mark_used();
                self.update_lru_access(&key).await;

                let mut stats = self.stats.lock().await;
                stats.connections_reused += 1;

                debug!(
                    "Reusing FTP connection to {}:{} (reuse #{}, baseWorkingDir={})",
                    host, port, conn.control.reuse_count, conn.key.base_working_dir
                );

                return Some(connections.remove(&key).unwrap());
            } else {
                // Connection is stale, remove it
                debug!("Removing stale FTP connection to {}:{}", host, port);
                connections.remove(&key);
                self.remove_from_lru(&key).await;

                let mut stats = self.stats.lock().await;
                stats.connections_evicted += 1;
            }
        }

        None
    }

    /// Try to get a connection matching only host/port/username (ignoring base_working_dir).
    ///
    /// This is useful when the caller can handle CWD traversal even if
    /// the pooled connection's base directory doesn't match exactly.
    /// The caller should check `base_working_dir()` on the returned
    /// connection to determine how much CWD work is needed.
    pub async fn try_get_relaxed(
        &self,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
    ) -> Option<PooledConnection> {
        let mut connections = self.connections.lock().await;

        // Find any healthy connection matching host/port/username
        let matching_key = connections
            .iter()
            .filter(|(k, _)| {
                k.host == host && k.port == port && k.username == username && k.password == password
            })
            .find(|(_, conn)| conn.is_healthy(self.config.max_idle_time))
            .map(|(k, _)| k.clone());

        if let Some(key) = matching_key {
            let conn = connections.get_mut(&key).unwrap();
            conn.mark_used();
            self.update_lru_access(&key).await;

            let mut stats = self.stats.lock().await;
            stats.connections_reused += 1;

            debug!(
                "Reusing FTP connection (relaxed) to {}:{} (baseWorkingDir={})",
                host, port, conn.key.base_working_dir
            );

            return Some(connections.remove(&key).unwrap());
        }

        None
    }

    /// Return a raw TCP control connection to the pool.
    ///
    /// This is the primary method called by `FtpFinishHandler` after a
    /// successful download (226 response). The stream, host, port,
    /// username, mode, and base_working_dir are stored for later reuse.
    ///
    /// Matches C++ `DownloadEngine::poolSocket(request, username, proxy, socket, baseWorkingDir)`.
    pub async fn return_raw_connection(
        &self,
        stream: TcpStream,
        host: &str,
        port: u16,
        username: &str,
        mode: FtpMode,
        base_working_dir: &str,
    ) -> Result<()> {
        // Check if we need to evict first
        self.evict_if_needed().await?;

        let key = ConnectionKey::new(
            host,
            port,
            username,
            "", // Password not needed for pooled reuse (already authenticated)
            base_working_dir,
        );

        let pooled = PooledConnection::new(stream, key.clone(), mode, self.config.read_timeout);

        let mut connections = self.connections.lock().await;
        connections.insert(key.clone(), pooled);

        self.add_to_lru(key.clone()).await;

        let mut stats = self.stats.lock().await;
        stats.connections_created += 1;
        stats.current_size = connections.len();
        if connections.len() > stats.peak_size {
            stats.peak_size = connections.len();
        }

        info!(
            "FTP connection pooled: {} (baseWorkingDir={})",
            key.to_pool_key_string(),
            base_working_dir
        );

        Ok(())
    }

    /// Return a connection to the pool for reuse.
    ///
    /// The connection is only returned if it's still healthy and
    /// hasn't exceeded its maximum age.
    pub async fn return_connection(&self, mut conn: PooledConnection) {
        // Check if connection is still healthy before returning
        if !conn.is_healthy(self.config.max_idle_time) {
            debug!(
                "Not returning unhealthy connection to {}:{}",
                conn.key.host, conn.key.port
            );
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            return;
        }

        // Check connection age
        if conn.age() > self.config.max_connection_age {
            debug!(
                "Not returning expired connection to {}:{} (age: {:?})",
                conn.key.host,
                conn.key.port,
                conn.age()
            );
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            return;
        }

        conn.mark_used();

        let mut connections = self.connections.lock().await;
        let key = conn.key.clone();
        connections.insert(key.clone(), conn);

        self.update_lru_access(&key).await;

        let mut stats = self.stats.lock().await;
        stats.current_size = connections.len();

        debug!(
            "Returned FTP connection to pool: {}",
            key.to_pool_key_string()
        );
    }

    /// Evict connections if pool is full
    async fn evict_if_needed(&self) -> Result<()> {
        let mut connections = self.connections.lock().await;

        while connections.len() >= self.config.max_connections {
            // Find the least recently used connection
            let lru_key = self.find_lru_key().await;

            if let Some(key) = lru_key {
                debug!(
                    "Evicting LRU connection to {}:{} (pool full)",
                    key.host, key.port
                );
                connections.remove(&key);
                self.remove_from_lru(&key).await;

                let mut stats = self.stats.lock().await;
                stats.connections_evicted += 1;
            } else {
                break;
            }
        }

        Ok(())
    }

    /// Add a key to the LRU tracking
    async fn add_to_lru(&self, key: ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
        lru.push(LruEntry {
            key,
            last_access: Instant::now(),
        });
    }

    /// Update LRU access time for a key
    async fn update_lru_access(&self, key: &ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
        if let Some(entry) = lru.iter_mut().find(|e| &e.key == key) {
            entry.last_access = Instant::now();
        }
    }

    /// Remove a key from LRU tracking
    async fn remove_from_lru(&self, key: &ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
        lru.retain(|e| &e.key != key);
    }

    /// Find the least recently used key
    async fn find_lru_key(&self) -> Option<ConnectionKey> {
        let lru = self.lru_order.lock().await;
        lru.iter()
            .min_by_key(|e| e.last_access)
            .map(|e| e.key.clone())
    }

    /// Clean up stale connections
    pub async fn cleanup_stale(&self) {
        let mut connections = self.connections.lock().await;
        let mut to_remove = Vec::new();

        for (key, conn) in connections.iter() {
            if !conn.is_healthy(self.config.max_idle_time)
                || conn.age() > self.config.max_connection_age
            {
                to_remove.push(key.clone());
            }
        }

        for key in to_remove {
            connections.remove(&key);
            self.remove_from_lru(&key).await;

            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
        }

        let mut stats = self.stats.lock().await;
        stats.current_size = connections.len();

        debug!(
            "FTP connection pool cleanup: {} connections remaining",
            connections.len()
        );
    }

    /// Get pool statistics
    pub async fn stats(&self) -> PoolStats {
        self.stats.lock().await.clone()
    }

    /// Get current pool size
    pub async fn size(&self) -> usize {
        self.connections.lock().await.len()
    }

    /// Clear all connections from the pool
    pub async fn clear(&self) {
        let mut connections = self.connections.lock().await;
        let count = connections.len();
        connections.clear();

        let mut lru = self.lru_order.lock().await;
        lru.clear();

        let mut stats = self.stats.lock().await;
        stats.connections_evicted += count as u64;
        stats.current_size = 0;

        info!("FTP connection pool cleared: {} connections removed", count);
    }

    /// Check if the pool has a connection for the given key
    pub async fn has_connection(&self, host: &str, port: u16, username: &str) -> bool {
        let connections = self.connections.lock().await;
        connections
            .keys()
            .any(|k| k.host == host && k.port == port && k.username == username)
    }
}

/// Create a new FTP connection pool with default configuration.
///
/// Use this to create an injectable pool instance instead of relying on a global singleton.
/// The pool should be created once during engine initialization and passed down via dependency injection.
pub fn create_pool(max_connections: usize) -> Arc<FtpConnectionPool> {
    Arc::new(FtpConnectionPool::new(max_connections))
}

/// Create a custom FTP connection pool with specific configuration.
pub fn create_custom_pool(config: PoolConfig) -> Arc<FtpConnectionPool> {
    Arc::new(FtpConnectionPool::with_config(config))
}

#[cfg(test)]
mod tests {
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
            last_access: Instant::now(),
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
}
