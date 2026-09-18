use tracing::{debug, info};

use crate::error::Result;
use crate::ftp::connection::FtpMode;

use super::{ConnectionKey, FtpConnectionPool, LruEntry, PooledConnection};

impl FtpConnectionPool {
    fn find_matching_key(
        connections: &std::collections::HashMap<ConnectionKey, PooledConnection>,
        host: &str,
        port: u16,
        username: &str,
        password: &str,
        base_working_dir: Option<&str>,
    ) -> Option<ConnectionKey> {
        let mut preauthenticated = None;
        for key in connections.keys() {
            if key.host != host
                || key.port != port
                || key.username != username
                || base_working_dir.is_some_and(|base| key.base_working_dir != base)
            {
                continue;
            }

            if key.password == password {
                return Some(key.clone());
            }
            if key.password.is_empty() {
                preauthenticated = Some(key.clone());
            }
        }
        preauthenticated
    }

    fn is_reusable(&self, connection: &PooledConnection) -> bool {
        connection.is_reusable(self.config.max_idle_time, self.config.max_connection_age)
    }

    /// Try to get an existing healthy connection from the pool.
    ///
    /// Returns `None` if no matching healthy connection is found.
    /// The connection is removed from the pool and must be returned via
    /// `return_connection()` when done.
    ///
    /// The lookup key includes `base_working_dir` -- if the caller's
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
        let mut connections = self.connections.lock().await;
        let key = Self::find_matching_key(
            &connections,
            host,
            port,
            username,
            password,
            Some(base_working_dir),
        )?;

        if let Some(conn) = connections.get_mut(&key) {
            if self.is_reusable(conn) {
                conn.mark_used();
                let reuse_count = conn.control.reuse_count;
                let connection_base = conn.key.base_working_dir.clone();
                let conn = connections.remove(&key).unwrap();
                self.remove_from_lru(&key).await;

                let mut stats = self.stats.lock().await;
                stats.connections_reused += 1;
                stats.current_size = connections.len();

                debug!(
                    "Reusing FTP connection to {}:{} (reuse #{}, baseWorkingDir={})",
                    host, port, reuse_count, connection_base
                );

                return Some(conn);
            } else {
                // Connection is stale, remove it
                debug!("Removing stale FTP connection to {}:{}", host, port);
                connections.remove(&key);
                self.remove_from_lru(&key).await;

                let mut stats = self.stats.lock().await;
                stats.connections_evicted += 1;
                stats.current_size = connections.len();
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

        let matching_key =
            Self::find_matching_key(&connections, host, port, username, password, None);

        if let Some(key) = matching_key {
            let conn = connections.get_mut(&key).unwrap();
            if self.is_reusable(conn) {
                conn.mark_used();
                let base_working_dir = conn.key.base_working_dir.clone();
                let conn = connections.remove(&key).unwrap();
                self.remove_from_lru(&key).await;

                let mut stats = self.stats.lock().await;
                stats.connections_reused += 1;
                stats.current_size = connections.len();

                debug!(
                    "Reusing FTP connection (relaxed) to {}:{} (baseWorkingDir={})",
                    host, port, base_working_dir
                );

                return Some(conn);
            }

            connections.remove(&key);
            self.remove_from_lru(&key).await;
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            stats.current_size = connections.len();
        }

        None
    }

    /// Return a raw TCP control connection to the pool.
    ///
    /// The stream, host, port, username, mode, and base_working_dir are
    /// stored for later reuse by the caller's download lifecycle.
    ///
    /// Matches C++ `DownloadEngine::poolSocket(request, username, proxy, socket, baseWorkingDir)`.
    pub async fn return_raw_connection(
        &self,
        stream: tokio::net::TcpStream,
        host: &str,
        port: u16,
        username: &str,
        mode: FtpMode,
        base_working_dir: &str,
    ) -> Result<()> {
        if self.config.max_connections == 0 {
            return Ok(());
        }

        // Check if we need to evict first.
        self.evict_if_needed().await;

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
        if self.config.max_connections == 0 {
            let mut stats = self.stats.lock().await;
            stats.connections_evicted += 1;
            return;
        }

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

        self.evict_if_needed().await;
        conn.mark_used();

        let mut connections = self.connections.lock().await;
        let key = conn.key.clone();
        connections.insert(key.clone(), conn);

        self.update_lru_access(&key).await;

        let mut stats = self.stats.lock().await;
        stats.current_size = connections.len();
        stats.peak_size = stats.peak_size.max(connections.len());

        debug!(
            "Returned FTP connection to pool: {}",
            key.to_pool_key_string()
        );
    }

    /// Evict connections if pool is full
    pub(crate) async fn evict_if_needed(&self) {
        if self.config.max_connections == 0 {
            return;
        }

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

        let mut stats = self.stats.lock().await;
        stats.current_size = connections.len();
    }

    /// Add a key to the LRU tracking
    pub(crate) async fn add_to_lru(&self, key: ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
        lru.retain(|entry| entry.key != key);
        lru.push(LruEntry {
            key,
            last_access: std::time::Instant::now(),
        });
    }

    /// Update LRU access time for a key
    pub(crate) async fn update_lru_access(&self, key: &ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
        if let Some(entry) = lru.iter_mut().find(|e| &e.key == key) {
            entry.last_access = std::time::Instant::now();
        } else {
            lru.push(LruEntry {
                key: key.clone(),
                last_access: std::time::Instant::now(),
            });
        }
    }

    /// Remove a key from LRU tracking
    pub(crate) async fn remove_from_lru(&self, key: &ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
        lru.retain(|e| &e.key != key);
    }

    /// Find the least recently used key
    pub(crate) async fn find_lru_key(&self) -> Option<ConnectionKey> {
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
            if !self.is_reusable(conn) {
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

    /// Check if the pool has a reusable connection for the given endpoint.
    pub async fn has_connection(&self, host: &str, port: u16, username: &str) -> bool {
        let connections = self.connections.lock().await;
        connections.iter().any(|(key, connection)| {
            key.host == host
                && key.port == port
                && key.username == username
                && self.is_reusable(connection)
        })
    }
}
