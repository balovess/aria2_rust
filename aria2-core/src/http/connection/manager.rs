//! HTTP connection manager
//!
//! Provides connection pool reuse, Keep-Alive management, LRU eviction,
//! idle timeout, and redirect following.
//!
//! # LRU Eviction
//!
//! Per-key idle connections live in a `VecDeque`: front = oldest (evicted
//! first), back = newest. Mirrors C++ `socketPool_` multimap ordering.

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::net::TcpStream;
use tokio::time::timeout;
use url::Url;

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::cookie_storage::{CookieJar, JarCookie};

use super::active_connection::{ActiveConnection, ConnectionPoolKey, ProxyInfo};
use super::types::{HttpConfig, HttpResponse};

/// HTTP connection manager
///
/// Provides connection acquisition, release, pool management, and redirect
/// following. Supports Keep-Alive reuse, LRU eviction, per-key idle limits,
/// and circular redirect detection.
pub struct HttpConnectionManager {
    config: HttpConfig,
    /// Idle connection pool: conn_id -> ActiveConnection.
    /// Only contains connections that are idle (released back via `put_back`).
    pool: HashMap<u64, ActiveConnection>,
    /// Per-key LRU index of idle connections.
    /// VecDeque front = oldest idle, back = newest idle.
    /// Only contains conn_ids for connections currently in `pool`.
    key_connections: HashMap<ConnectionPoolKey, VecDeque<u64>>,
    /// Total live connection count (idle in pool + in-use by callers)
    active_count: usize,
    /// Connection ID generator
    id_counter: AtomicU64,
    /// Maximum redirect hops
    max_redirects: u32,
    /// Optional cookie jar for automatic cookie management
    cookie_jar: Option<CookieJar>,
}

impl HttpConnectionManager {
    /// Create a new HTTP connection manager
    pub fn new(config: &HttpConfig) -> Self {
        Self {
            config: config.clone(),
            pool: HashMap::new(),
            key_connections: HashMap::new(),
            active_count: 0,
            id_counter: AtomicU64::new(1),
            max_redirects: crate::constants::HTTP_DEFAULT_MAX_REDIRECTS as u32,
            cookie_jar: None,
        }
    }

    // ==================== Accessors ====================

    /// Maximum concurrent connections configuration
    pub fn max_connections(&self) -> usize {
        self.config.max_connections
    }

    /// Current total live connection count (idle + in-use)
    pub fn active_count(&self) -> usize {
        self.active_count
    }

    /// Current idle connection pool size
    pub fn pool_size(&self) -> usize {
        self.pool.len()
    }

    /// Number of idle connections for a specific pool key
    pub fn idle_count_for_key(&self, key: &ConnectionPoolKey) -> usize {
        self.key_connections.get(key).map_or(0, |dq| dq.len())
    }

    /// Maximum idle connections per host configuration
    pub fn max_idle_per_host(&self) -> usize {
        self.config.max_idle_per_host
    }

    // ==================== Pool Operations (mirrors C++ API) ====================

    /// Acquire a connection — reuse idle or create new.
    ///
    /// Mirrors C++ `popPooledSocket()` + new connection creation.
    /// On reuse, the oldest valid idle connection is returned (LRU front).
    pub async fn acquire(&mut self, url: &Url, proxy: Option<&ProxyInfo>) -> Result<ActiveConnection> {
        let host = Self::extract_host(url);
        let pool_key = ConnectionPoolKey {
            target: host.clone(),
            proxy: proxy.cloned(),
        };

        // Try to reuse an idle connection from the pool
        if let Some(conn) = self.pop_pooled_connection(&pool_key)? {
            tracing::debug!("Reused connection: id={}, key={:?}", conn.id, pool_key);
            return Ok(conn);
        }

        // Check if max connection limit has been reached
        if self.active_count >= self.config.max_connections {
            // Try to evict expired connections to free slots
            self.check_timeout();

            if self.active_count >= self.config.max_connections {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Max connection limit reached: {} (key={:?})",
                            self.config.max_connections, pool_key
                        ),
                    },
                ));
            }
        }

        // Create a new connection
        self.create_new_connection(url, &host, pool_key).await
    }

    /// Return a connection to the idle pool (mirrors C++ `putBack()`).
    ///
    /// Takes ownership of the connection. It goes to the back of the LRU
    /// deque (newest). If per-key idle count exceeds `max_idle_per_host`,
    /// the oldest is evicted first. Invalid connections are discarded.
    pub async fn put_back(&mut self, mut conn: ActiveConnection) {
        if !conn.is_valid() {
            tracing::debug!("Connection no longer valid on put_back: id={}", conn.id);
            self.active_count = self.active_count.saturating_sub(1);
            return;
        }

        let conn_id = conn.id;
        let pool_key = conn.pool_key.clone();

        // Mark as idle and place in pool
        conn.mark_pooled();
        self.pool.insert(conn_id, conn);

        // Add to back of LRU deque (newest idle)
        self.key_connections
            .entry(pool_key.clone())
            .or_default()
            .push_back(conn_id);

        // Enforce per-key idle limit (evict oldest = front of deque)
        self.enforce_idle_limit(&pool_key).await;

        tracing::debug!("Put back connection to pool: id={}, key={:?}", conn_id, pool_key);
    }

    /// Legacy alias for `put_back()`.
    pub async fn release(&mut self, conn: ActiveConnection) {
        self.put_back(conn).await;
    }

    /// Retrieve a pooled idle connection for a specific key without creating new.
    ///
    /// Mirrors C++ `popPooledSocket()`. Returns `None` if no valid idle
    /// connection exists for the given key.
    pub fn get_connection(&mut self, pool_key: &ConnectionPoolKey) -> Result<Option<ActiveConnection>> {
        self.pop_pooled_connection(pool_key)
    }

    /// Evict all idle connections that exceeded the idle timeout.
    ///
    /// Mirrors C++ `evictSocketPool()`. Call periodically.
    /// Returns the number of connections evicted.
    pub fn check_timeout(&mut self) -> usize {
        if self.pool.is_empty() {
            return 0;
        }

        let idle_timeout = self.config.idle_timeout;
        let mut evicted_keys: Vec<(u64, ConnectionPoolKey)> = Vec::new();

        for (&conn_id, conn) in &self.pool {
            if conn.is_idle_timeout(idle_timeout) {
                evicted_keys.push((conn_id, conn.pool_key.clone()));
            }
        }

        let count = evicted_keys.len();
        for (conn_id, pool_key) in evicted_keys {
            if let Some(mut conn) = self.pool.remove(&conn_id) {
                tracing::debug!(
                    "Evicted idle-timeout connection: id={}, idle={:.2}s",
                    conn_id,
                    conn.pooled_at.map_or(0.0, |t| t.elapsed().as_secs_f64())
                );
                std::mem::drop(conn.shutdown());
                self.active_count = self.active_count.saturating_sub(1);
                self.remove_from_key_deque(&pool_key, conn_id);
            }
        }

        if count > 0 {
            tracing::info!("Evicted {} idle-timeout connections", count);
        }
        count
    }

    /// Close all idle connections (mirrors C++ `releaseAll()`).
    ///
    /// Active (in-use) connections are NOT affected.
    pub async fn release_all(&mut self) {
        let count = self.pool.len();
        for (_, mut conn) in self.pool.drain() {
            let _ = conn.shutdown().await;
        }
        self.key_connections.clear();
        self.active_count = self.active_count.saturating_sub(count);

        if count > 0 {
            tracing::info!("Released all {} idle connections", count);
        }
    }

    /// Full cleanup — close all connections and reset state.
    pub async fn cleanup(&mut self) {
        for (_, mut conn) in self.pool.drain() {
            let _ = conn.shutdown().await;
        }
        self.key_connections.clear();
        self.active_count = 0;
        tracing::info!("Connection pool cleaned up");
    }

    /// Force close a specific connection by ID.
    pub async fn close_connection(&mut self, conn_id: u64) {
        if let Some(mut conn) = self.pool.remove(&conn_id) {
            let pool_key = conn.pool_key.clone();
            let _ = conn.shutdown().await;
            self.active_count = self.active_count.saturating_sub(1);
            self.remove_from_key_deque(&pool_key, conn_id);
            tracing::debug!("Force closed connection: id={}", conn_id);
        }
    }

    // ==================== Redirect Following ====================

    /// Follow HTTP redirects with circular detection.
    pub fn follow_redirects(
        &self,
        response: &HttpResponse,
        current_url: &Url,
        redirect_chain: &HashSet<Url>,
        redirect_count: u32,
    ) -> Result<Url> {
        if !response.is_redirect() {
            return Err(Aria2Error::Parse(format!(
                "Non-redirect response code: {}", response.status_code
            )));
        }
        if redirect_count >= self.max_redirects {
            return Err(Aria2Error::Network(format!(
                "Max redirect count exceeded: {}", self.max_redirects
            )));
        }
        let location = response
            .location()
            .ok_or_else(|| Aria2Error::Parse("Missing Location header".to_string()))?;
        let new_url = current_url
            .join(location)
            .map_err(|e| Aria2Error::Parse(format!("Failed to parse redirect URL: {}", e)))?;
        if redirect_chain.contains(&new_url) {
            return Err(Aria2Error::Network(format!(
                "Circular redirect detected: {}", new_url
            )));
        }
        tracing::info!(
            "Following redirect: {} -> {} ({}/{})",
            current_url, new_url, redirect_count + 1, self.max_redirects
        );
        Ok(new_url)
    }

    /// Iteratively follow HTTP redirects with loop detection.
    pub async fn follow_redirects_iterative<F, Fut>(
        &self,
        initial_url: &Url,
        mut get_response: F,
    ) -> Result<HttpResponse>
    where
        F: FnMut(&Url) -> Fut,
        Fut: std::future::Future<Output = Result<HttpResponse>>,
    {
        const MAX_REDIRECTS: u8 = crate::constants::HTTP_DEFAULT_MAX_REDIRECTS as u8;
        let mut current_url = initial_url.clone();
        let mut seen_urls = HashSet::<String>::new();

        for iteration in 0..MAX_REDIRECTS {
            let url_str = current_url.to_string();
            if !seen_urls.insert(url_str.clone()) {
                return Err(Aria2Error::Network(format!("Redirect loop detected: {}", url_str)));
            }
            let resp = get_response(&current_url).await?;
            if !resp.is_redirect() {
                return Ok(resp);
            }
            let location = resp.location().ok_or_else(|| {
                Aria2Error::Network("Missing Location header in redirect response".into())
            })?;
            current_url = current_url
                .join(location)
                .map_err(|e| Aria2Error::Parse(format!("Failed to parse redirect URL: {}", e)))?;
            tracing::info!("Following redirect: iteration {}/{}", iteration + 1, MAX_REDIRECTS);
        }

        Err(Aria2Error::Network(format!(
            "Too many redirects (>{}), last URL: {}", MAX_REDIRECTS, current_url
        )))
    }

    // ==================== Range Headers ====================

    /// Build a Range request header per RFC 7233.
    pub fn build_range_header(&self, start: u64, end: Option<u64>) -> String {
        match end {
            Some(e) => format!("bytes={}-{}", start, e),
            None => format!("bytes={}-", start),
        }
    }

    /// Parse Content-Range response header. Returns `(start, end, total)`.
    pub fn parse_content_range(&self, header: &str) -> Option<(u64, u64, u64)> {
        let header = header.trim();
        if !header.starts_with("bytes ") { return None; }
        let parts: Vec<&str> = header[6..].split('/').collect();
        if parts.len() != 2 { return None; }
        let rv: Vec<&str> = parts[0].split('-').collect();
        if rv.len() != 2 { return None; }
        let start: u64 = rv[0].trim().parse().ok()?;
        let end: u64 = rv[1].trim().parse().ok()?;
        let total = match parts[1].trim() {
            "*" => u64::MAX,
            s => s.parse().ok()?,
        };
        Some((start, end, total))
    }

    // ==================== Cookie Jar Integration ====================

    /// Set the cookie jar for automatic cookie management.
    pub fn set_cookie_jar(&mut self, jar: Option<CookieJar>) {
        self.cookie_jar = jar;
    }

    /// Get a reference to the current cookie jar.
    pub fn cookie_jar(&self) -> &Option<CookieJar> {
        &self.cookie_jar
    }

    /// Get a mutable reference to the current cookie jar.
    pub fn cookie_jar_mut(&mut self) -> &mut Option<CookieJar> {
        &mut self.cookie_jar
    }

    /// Attach matching cookies from the jar to an HTTP request.
    pub fn attach_cookies_to_request(&self, url: &Url) -> Option<String> {
        let jar = self.cookie_jar.as_ref()?;
        let is_https = url.scheme() == "https";
        jar.cookie_header_for_url(url.as_str(), is_https)
    }

    /// Extract cookies from response Set-Cookie headers and store in the jar.
    pub fn extract_cookies_from_response(
        &mut self,
        response_headers: &[(String, String)],
        _request_url: &Url,
    ) -> usize {
        let jar = match &mut self.cookie_jar {
            Some(j) => j,
            None => return 0,
        };
        let mut stored = 0;
        for (name, value) in response_headers {
            if name.eq_ignore_ascii_case("set-cookie")
                && let Some(cookie) = JarCookie::parse_set_cookie(value)
            {
                jar.store(cookie);
                stored += 1;
                tracing::debug!(
                    "Extracted cookie from Set-Cookie: {}",
                    &value[..value.len().min(80)]
                );
            }
        }
        stored
    }

    // ==================== Private Helpers ====================

    /// Extract host identifier from URL (host:port)
    pub(super) fn extract_host(url: &Url) -> String {
        match url.port_or_known_default() {
            Some(port) => format!(
                "{}:{}",
                url.host_str().unwrap_or(crate::constants::DEFAULT_HOST),
                port
            ),
            None => url
                .host_str()
                .unwrap_or(crate::constants::DEFAULT_HOST)
                .to_string(),
        }
    }

    /// Pop the oldest valid idle connection for the key.
    /// Matches C++ `findSocketPoolEntry()`. Skips timed-out/invalid.
    fn pop_pooled_connection(&mut self, pool_key: &ConnectionPoolKey) -> Result<Option<ActiveConnection>> {
        let deque = match self.key_connections.get_mut(pool_key) {
            Some(dq) => dq,
            None => return Ok(None),
        };

        // Walk from front (oldest) to find first valid, non-expired connection
        let mut candidate_ids: Vec<u64> = Vec::new();
        while let Some(&conn_id) = deque.front() {
            candidate_ids.push(conn_id);
            deque.pop_front();

            if let Some(mut conn) = self.pool.remove(&conn_id) {
                if conn.is_valid() && !conn.is_idle_timeout(self.config.idle_timeout) {
                    // Valid connection — mark as in-use and return
                    conn.mark_in_use();
                    conn.touch();

                    tracing::debug!(
                        "Reused idle connection: id={}, idle={:.2}s",
                        conn_id,
                        conn.pooled_at.map_or(0.0, |t| t.elapsed().as_secs_f64())
                    );
                    return Ok(Some(conn));
                } else {
                    // Expired or invalid — discard
                    let reason = if !conn.is_valid() { "invalid" } else { "timed-out" };
                    tracing::debug!("Discarded {} idle connection: id={}", reason, conn_id);
                    std::mem::drop(conn.shutdown());
                    self.active_count = self.active_count.saturating_sub(1);
                }
            }
            // Connection not in pool (shouldn't happen) — just skip
        }

        // All candidates were invalid/expired; deque is now empty
        if deque.is_empty() {
            self.key_connections.remove(pool_key);
        }

        Ok(None)
    }

    /// Create a new TCP connection.
    async fn create_new_connection(
        &mut self,
        url: &Url,
        host: &str,
        pool_key: ConnectionPoolKey,
    ) -> Result<ActiveConnection> {
        let addr = Self::resolve_address(url)?;

        let stream = timeout(self.config.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("TCP connection failed ({}): {}", addr, e),
                })
            })?;

        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!("Failed to set nodelay: {}", e);
        }

        let conn_id = self.id_counter.fetch_add(1, Ordering::SeqCst);

        let conn = ActiveConnection {
            id: conn_id,
            stream,
            host: host.to_string(),
            last_used: Instant::now(),
            pooled_at: None, // In use, not idle
            pool_key: pool_key.clone(),
        };

        self.active_count += 1;
        // Do NOT add to key_connections — connection is in use, not idle

        tracing::info!(
            "Created new connection: id={}, host={}, active={}/{}",
            conn_id, host, self.active_count, self.config.max_connections
        );

        Ok(conn)
    }

    /// Resolve URL to SocketAddr
    fn resolve_address(url: &Url) -> Result<SocketAddr> {
        let host = url
            .host_str()
            .ok_or_else(|| Aria2Error::Parse("URL missing hostname".to_string()))?;

        let port = url
            .port_or_known_default()
            .ok_or_else(|| Aria2Error::Parse("Unable to determine port number".to_string()))?;

        let addr_str = format!("{}:{}", host, port);
        addr_str
            .parse::<SocketAddr>()
            .map_err(|e| Aria2Error::Parse(format!("Failed to resolve address: {}", e)))
    }

    /// Enforce per-key idle limit by evicting oldest (front of deque).
    async fn enforce_idle_limit(&mut self, pool_key: &ConnectionPoolKey) {
        let max_idle = self.config.max_idle_per_host;
        if max_idle == 0 {
            return; // 0 = unlimited
        }

        while self.idle_count_for_key(pool_key) > max_idle {
            let evict_id = match self.key_connections.get_mut(pool_key) {
                Some(dq) => match dq.pop_front() {
                    Some(id) => id,
                    None => break,
                },
                None => break,
            };

            if let Some(mut conn) = self.pool.remove(&evict_id) {
                tracing::debug!(
                    "LRU evicted oldest idle connection: id={}, key={:?} (limit={})",
                    evict_id, pool_key, max_idle
                );
                let _ = conn.shutdown().await;
                self.active_count = self.active_count.saturating_sub(1);
            }
        }

        // Clean up empty deque entry
        if self.idle_count_for_key(pool_key) == 0 {
            self.key_connections.remove(pool_key);
        }
    }

    /// Remove a conn_id from the key's LRU deque. O(n) but deque is small.
    fn remove_from_key_deque(&mut self, pool_key: &ConnectionPoolKey, conn_id: u64) {
        if let Some(dq) = self.key_connections.get_mut(pool_key) {
            dq.retain(|&id| id != conn_id);
            if dq.is_empty() {
                self.key_connections.remove(pool_key);
            }
        }
    }
}

impl Drop for HttpConnectionManager {
    fn drop(&mut self) {
        for (_, conn) in self.pool.drain() {
            drop(conn);
        }
        self.key_connections.clear();
    }
}

impl std::fmt::Debug for HttpConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpConnectionManager")
            .field("max_connections", &self.config.max_connections)
            .field("connect_timeout", &self.config.connect_timeout)
            .field("read_timeout", &self.config.read_timeout)
            .field("write_timeout", &self.config.write_timeout)
            .field("idle_timeout", &self.config.idle_timeout)
            .field("max_idle_per_host", &self.config.max_idle_per_host)
            .field("active_count", &self.active_count)
            .field("pool_size", &self.pool.len())
            .field("cookie_jar_set", &self.cookie_jar.is_some())
            .finish()
    }
}
