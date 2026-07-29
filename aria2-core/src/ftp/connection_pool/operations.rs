
use tracing::{debug, info};

use crate::error::Result;
use crate::ftp::connection::FtpMode;

use super::{ConnectionKey, FtpConnectionPool, LruEntry, PooledConnection};

impl FtpConnectionPool {
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
        stream: tokio::net::TcpStream,
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
    pub(crate) async fn evict_if_needed(&self) -> Result<()> {
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
    pub(crate) async fn add_to_lru(&self, key: ConnectionKey) {
        let mut lru = self.lru_order.lock().await;
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

