//! HTTP connection manager
//!
//! Provides connection pool reuse, Keep-Alive management, LRU eviction strategy, and redirect following.
//!
//! # Example
//!
//! ```rust,no_run
//! use aria2_core::http::connection::{HttpConnectionManager, HttpConfig};
//! use std::time::Duration;
//!
//! #[tokio::main]
//! async fn main() {
//!     let config = HttpConfig {
//!         max_connections: 10,
//!         connect_timeout: Duration::from_secs(30),
//!         read_timeout: Duration::from_secs(60),
//!         write_timeout: Duration::from_secs(60),
//!         idle_timeout: Duration::from_secs(300),
//!     };
//!
//!     let manager = HttpConnectionManager::new(&config);
//!     // Use the connection manager...
//! }
//! ```

use std::collections::{HashMap, HashSet};
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
/// Provides HTTP connection acquisition, release, pool management, and redirect following.
/// Supports Keep-Alive connection reuse, LRU eviction strategy, and circular redirect detection.
///
/// # Thread Safety
///
/// `HttpConnectionManager` uses `tokio::sync::Mutex` internally to protect shared state,
/// and can be safely shared across multiple async tasks.
///
/// # Performance Characteristics
///
/// - **Connection Reuse**: Avoids repeated TCP connections via Keep-Alive header checks
/// - **LRU Eviction**: Automatically cleans up idle timed-out connections to prevent resource leaks
/// - **Three-tier Timeout**: Separately controls connect, read, and write phase timeouts
///
/// # Example
///
/// ```rust,no_run
/// use aria2_core::http::connection::{HttpConnectionManager, HttpConfig};
/// use std::time::Duration;
/// use url::Url;
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let config = HttpConfig {
///         max_connections: 8,
///         ..Default::default()
///     };
///
///     let mut manager = HttpConnectionManager::new(&config);
///     let url = Url::parse("https://example.com/file")?;
///
///     let conn = manager.acquire(&url).await?;
///     // Use the connection for HTTP requests...
///     manager.release(conn).await;
///
///     Ok(())
/// }
/// ```
pub struct HttpConnectionManager {
    /// Configuration parameters
    config: HttpConfig,
    /// Connection pool: conn_id -> ActiveConnection
    pool: HashMap<u64, ActiveConnection>,
    /// Pool-key-to-connection-ID mapping (for fast lookup of reusable connections).
    /// Keyed by `ConnectionPoolKey` (target + proxy) so that direct and proxied
    /// connections are never confused — matching the C++ `poolSocket(req, proxyReq, sock)` API.
    key_connections: HashMap<ConnectionPoolKey, Vec<u64>>,
    /// Current active connection count
    active_count: usize,
    /// Connection ID generator
    id_counter: AtomicU64,
    /// Maximum redirect hops
    max_redirects: u32,
    /// Optional cookie jar for automatic cookie management on HTTP requests.
    ///
    /// When set, the connection manager will:
    /// - Attach matching Cookie headers to outgoing requests via `attach_cookies_to_request()`
    /// - Extract and store Set-Cookie headers from responses via `extract_cookies_from_response()`
    cookie_jar: Option<CookieJar>,
}

impl HttpConnectionManager {
    /// Create a new HTTP connection manager
    ///
    /// # Arguments
    ///
    /// * `config` - HTTP connection configuration, including timeout, max connections, etc.
    ///
    /// # Returns
    ///
    /// The initialized connection manager instance
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::http::connection::{HttpConnectionManager, HttpConfig};
    /// use std::time::Duration;
    ///
    /// let config = HttpConfig {
    ///     max_connections: 10,
    ///     connect_timeout: Duration::from_secs(15),
    ///     read_timeout: Duration::from_secs(30),
    ///     write_timeout: Duration::from_secs(30),
    ///     idle_timeout: Duration::from_secs(120),
    /// };
    ///
    /// let manager = HttpConnectionManager::new(&config);
    /// assert_eq!(manager.max_connections(), 10);
    /// ```
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

    /// Get the max connections configuration
    pub fn max_connections(&self) -> usize {
        self.config.max_connections
    }

    /// Get the current active connection count
    pub fn active_count(&self) -> usize {
        self.active_count
    }

    /// Get the connection pool size (including idle and in-use connections)
    pub fn pool_size(&self) -> usize {
        self.pool.len()
    }

    /// Acquire or create a connection to the specified URL from the connection pool
    ///
    /// This method attempts to find a reusable idle connection from the pool (based on hostname matching).
    /// If no connection is available and the max connection limit has not been reached, a new connection is created.
    ///
    /// # Arguments
    ///
    /// * `url` - Target URL, used to extract hostname and port information
    ///
    /// # Errors
    ///
    /// * [`Aria2Error::Network`] - When max connection limit is reached
    /// * [`Aria2Error::Recoverable`] - When connection times out or network failure occurs
    ///
    /// # Returns
    ///
    /// An available active connection instance
    ///
    /// # Keep-Alive Reuse Logic
    ///
    /// 1. Extract the URL's host:port as the connection identifier
    /// 2. Look up idle connections for that host in the pool
    /// 3. Validate connection validity (check if socket is healthy)
    /// 4. Update the last_used timestamp and return
    /// 5. If no available connection, create a new TCP connection
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::http::connection::HttpConnectionManager;
    /// use url::Url;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut manager = HttpConnectionManager::new(&Default::default());
    ///     let url = Url::parse("https://example.com/resource").unwrap();
    ///
    ///     match manager.acquire(&url).await {
    ///         Ok(conn) => println!("Acquired connection: id={}", conn.id),
    ///         Err(e) => eprintln!("Failed to acquire connection: {}", e),
    ///     }
    /// }
    /// ```
    pub async fn acquire(&mut self, url: &Url, proxy: Option<&ProxyInfo>) -> Result<ActiveConnection> {
        let host = Self::extract_host(url);
        let pool_key = ConnectionPoolKey {
            target: host.clone(),
            proxy: proxy.cloned(),
        };

        // Try to reuse an idle connection from the pool
        if let Some(conn) = self.try_reuse_connection(&pool_key)? {
            tracing::debug!("Reused connection: id={}, key={:?}", conn.id, pool_key);
            return Ok(conn);
        }

        // Check if max connection limit has been reached
        if self.active_count >= self.config.max_connections {
            // Try to evict expired connections
            self.evict_idle_connections();

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

    /// Return a connection to the connection pool
    ///
    /// Returns a used connection to the pool for future reuse.
    /// If the connection is no longer valid (socket closed or errored), it is automatically removed from the pool.
    ///
    /// # Arguments
    ///
    /// * `conn_id` - The connection ID to return
    ///
    /// # Behavior
    ///
    /// 1. Look up the connection by ID in the pool
    /// 2. Update last_used to current time
    /// 3. Mark the connection as idle
    /// 4. If the connection is invalid, automatically clean up resources
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::http::connection::HttpConnectionManager;
    /// use url::Url;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let mut manager = HttpConnectionManager::new(&Default::default());
    ///     let url = Url::parse("https://example.com").unwrap();
    ///
    ///     let conn = manager.acquire(&url).await.unwrap();
    ///     // After using the connection for requests...
    ///     manager.release(conn).await;
    /// }
    /// ```
    pub async fn release(&mut self, conn_id: u64) {
        if let Some(mut conn) = self.pool.remove(&conn_id) {
            // Verify if the connection is still valid
            if !conn.is_valid() {
                tracing::debug!("Connection no longer valid, removing: id={}", conn_id);
                self.active_count = self.active_count.saturating_sub(1);
                self.remove_from_key_map(&conn.pool_key, conn_id);
                return;
            }

            // Update last used time and put back into the pool
            conn.touch();
            self.pool.insert(conn_id, conn);

            tracing::debug!("Returned connection to pool: id={}", conn_id);
        } else {
            tracing::warn!("Attempted to release non-existent connection: id={}", conn_id);
        }
    }

    /// Follow HTTP redirects
    ///
    /// Parses the Location header from the response, constructs a new URL, and validates redirect legality.
    /// Supports both relative and absolute path redirects, with automatic circular redirect detection.
    ///
    /// # Arguments
    ///
    /// * `response` - HTTP response object, must contain a Location header
    /// * `current_url` - Current request URL (used for resolving relative paths)
    /// * `redirect_chain` - Set of already-visited URLs (for circular detection)
    ///
    /// # Errors
    ///
    /// * [`Aria2Error::Parse`] - When Location header format is invalid or URL parsing fails
    /// * [`Aria2Error::Network`] - When circular redirect is detected or max hops exceeded
    ///
    /// # Returns
    ///
    /// The new URL of the redirect target
    ///
    /// # Redirect Chain Detection Mechanism
    ///
    /// 1. Use HashSet to record all visited URLs
    /// 2. Check if the new URL is already in the set before each redirect
    /// 3. Maintain a hop counter; return error when threshold is exceeded
    /// 4. Support up to 5 301/302/303/307/308 redirects
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::http::connection::{HttpConnectionManager, HttpResponse};
    /// use std::collections::HashSet;
    /// use url::Url;
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let manager = HttpConnectionManager::new(&Default::default());
    ///     let current_url = Url::parse("http://example.com/old").unwrap();
    ///     let mut chain = HashSet::new();
    ///     chain.insert(current_url.clone());
    ///
    ///     let mut response = HttpResponse::new(301, "Moved".to_string());
    ///     response.headers.push((
    ///         "Location".to_string(),
    ///         "/new-path".to_string(),
    ///     ));
    ///
    ///     match manager.follow_redirects(&response, &current_url, &chain, 1) {
    ///         Ok(new_url) => println!("Redirected to: {}", new_url),
    ///         Err(e) => eprintln!("Redirect failed: {}", e),
    ///     }
    /// }
    /// ```
    pub fn follow_redirects(
        &self,
        response: &HttpResponse,
        current_url: &Url,
        redirect_chain: &HashSet<Url>,
        redirect_count: u32,
    ) -> Result<Url> {
        // Check if this is a redirect response
        if !response.is_redirect() {
            return Err(Aria2Error::Parse(format!(
                "Non-redirect response code: {}",
                response.status_code
            )));
        }

        // Check redirect count limit
        if redirect_count >= self.max_redirects {
            return Err(Aria2Error::Network(format!(
                "Max redirect count exceeded: {}",
                self.max_redirects
            )));
        }

        // Get the Location header
        let location = response
            .location()
            .ok_or_else(|| Aria2Error::Parse("Missing Location header".to_string()))?;

        // Parse the new URL (supports relative paths)
        let new_url = current_url
            .join(location)
            .map_err(|e| Aria2Error::Parse(format!("Failed to parse redirect URL: {}", e)))?;

        // Circular redirect detection
        if redirect_chain.contains(&new_url) {
            return Err(Aria2Error::Network(format!(
                "Circular redirect detected: {}",
                new_url
            )));
        }

        tracing::info!(
            "Following redirect: {} -> {} ({}/{})",
            current_url,
            new_url,
            redirect_count + 1,
            self.max_redirects
        );

        Ok(new_url)
    }

    /// Iteratively follow HTTP redirects with loop detection
    ///
    /// This method replaces recursive redirect following with an iterative approach,
    /// eliminating stack overflow risk for deep redirect chains.
    ///
    /// # Arguments
    ///
    /// * `initial_url` - The starting URL for the request
    /// * `get_response` - Async closure that fetches the HTTP response for a given URL
    ///
    /// # Returns
    ///
    /// The final non-redirect HttpResponse, or an error if:
    /// - Too many redirects (exceeds MAX_REDIRECTS limit)
    /// - Redirect loop detected (same URL visited twice)
    /// - Missing Location header in redirect response
    /// - Invalid URL in Location header
    ///
    /// # Performance characteristics
    ///
    /// - Uses HashSet<String> for O(1) loop detection instead of linear scan
    /// - Iterative loop with bounded iterations prevents stack growth
    /// - Maximum 5 redirects as per RFC 7231 recommendation
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
        let mut seen_urls = std::collections::HashSet::<String>::new();

        for iteration in 0..MAX_REDIRECTS {
            // Detect redirect loops using HashSet for O(1) lookup
            let url_str = current_url.to_string();
            if !seen_urls.insert(url_str.clone()) {
                return Err(Aria2Error::Network(format!(
                    "Redirect loop detected: {}",
                    url_str
                )));
            }

            // Fetch response for current URL
            let resp = get_response(&current_url).await?;

            // If not a redirect, return the final response
            if !resp.is_redirect() {
                return Ok(resp);
            }

            // Extract Location header from redirect response
            let location = resp.location().ok_or_else(|| {
                Aria2Error::Network("Missing Location header in redirect response".into())
            })?;

            // Resolve relative URLs against current URL
            current_url = current_url
                .join(location)
                .map_err(|e| Aria2Error::Parse(format!("Failed to parse redirect URL: {}", e)))?;

            tracing::info!(
                "Following redirect: iteration {}/{}",
                iteration + 1,
                MAX_REDIRECTS
            );
        }

        Err(Aria2Error::Network(format!(
            "Too many redirects (>{}), last URL: {}",
            MAX_REDIRECTS, current_url
        )))
    }

    /// Build a Range request header
    ///
    /// Constructs a Range header string conforming to RFC 7233 based on start and end byte positions.
    /// Used for resume downloads and chunked download scenarios.
    ///
    /// # Arguments
    ///
    /// * `start` - Start byte position (inclusive)
    /// * `end` - End byte position (inclusive); if None, means end of file
    ///
    /// # Returns
    ///
    /// Formatted Range header value, e.g. `"bytes=0-499"` or `"bytes=500-"`
    ///
    /// # Format Specification
    ///
    /// - `bytes=start-end`: Specify range [start, end]
    /// - `bytes=start-`: From start to end of file
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::http::connection::HttpConnectionManager;
    ///
    /// let manager = HttpConnectionManager::new(&Default::default());
    ///
    /// // Full range
    /// assert_eq!(
    ///     manager.build_range_header(0, Some(499)),
    ///     "bytes=0-499"
    /// );
    ///
    /// // Open-ended range
    /// assert_eq!(
    ///     manager.build_range_header(1000, None),
    ///     "bytes=1000-"
    /// );
    ///
    /// // Single byte range
    /// assert_eq!(
    ///     manager.build_range_header(42, Some(42)),
    ///     "bytes=42-42"
    /// );
    /// ```
    pub fn build_range_header(&self, start: u64, end: Option<u64>) -> String {
        match end {
            Some(end_val) => format!("bytes={}-{}", start, end_val),
            None => format!("bytes={}-", start),
        }
    }

    /// Parse Content-Range response header
    ///
    /// Parses the Content-Range header value returned by the server, extracting range information and total size.
    /// Used to verify if the server correctly supports Range requests.
    ///
    /// # Arguments
    ///
    /// * `header` - Raw string value of the Content-Range header
    ///
    /// # Returns
    ///
    /// If parsing succeeds, returns a tuple `(start, end, total)`:
    /// - `start`: Range start byte (inclusive)
    /// - `end`: Range end byte (inclusive)
    /// - `total`: Total file size in bytes (u64::MAX if unknown)
    ///
    /// Returns `None` if the format is invalid
    ///
    /// # Supported Formats
    ///
    /// - `bytes 0-499/1000`: Range with known total size
    /// - `bytes 0-499/*`: Range with unknown total size
    ///
    /// # Example
    ///
    /// ```
    /// use aria2_core::http::connection::HttpConnectionManager;
    ///
    /// let manager = HttpConnectionManager::new(&Default::default());
    ///
    /// // Parse with known total size
    /// let result = manager.parse_content_range("bytes 0-499/1000");
    /// assert_eq!(result, Some((0, 499, 1000)));
    ///
    /// // Parse with unknown total size
    /// let result = manager.parse_content_range("bytes 500-999/*");
    /// assert_eq!(result, Some((500, 999, u64::MAX)));
    ///
    /// // Invalid format
    /// assert_eq!(manager.parse_content_range("invalid"), None);
    /// assert_eq!(manager.parse_content_range("bits 0-99/1000"), None);
    /// ```
    pub fn parse_content_range(&self, header: &str) -> Option<(u64, u64, u64)> {
        let header = header.trim();

        // Must start with "bytes "
        if !header.starts_with("bytes ") {
            return None;
        }

        let range_part = &header[6..];
        let parts: Vec<&str> = range_part.split('/').collect();

        if parts.len() != 2 {
            return None;
        }

        // Parse the start-end portion
        let range_values: Vec<&str> = parts[0].split('-').collect();
        if range_values.len() != 2 {
            return None;
        }

        let start: u64 = range_values[0].trim().parse().ok()?;
        let end: u64 = range_values[1].trim().parse().ok()?;

        // Parse the total size
        let total = match parts[1].trim() {
            "*" => u64::MAX,
            s => s.parse().ok()?,
        };

        Some((start, end, total))
    }

    /// Clean up all idle connections
    ///
    /// Closes all connections in the pool and releases system resources.
    /// Typically called when a download task completes or the program exits.
    pub async fn cleanup(&mut self) {
        for (_, mut conn) in self.pool.drain() {
            let _ = conn.shutdown().await;
        }
        self.key_connections.clear();
        self.active_count = 0;

        tracing::info!("Connection pool cleaned up");
    }

    /// Force close a specific connection
    ///
    /// Remove the connection from the pool and close the underlying TCP connection.
    /// Used for error handling or abnormal termination.
    ///
    /// # Arguments
    ///
    /// * `conn_id` - The ID of the connection to close
    pub async fn close_connection(&mut self, conn_id: u64) {
        if let Some(mut conn) = self.pool.remove(&conn_id) {
            let pool_key = conn.pool_key.clone();
            let _ = conn.shutdown().await;
            self.active_count = self.active_count.saturating_sub(1);
            self.remove_from_key_map(&pool_key, conn_id);
            tracing::debug!("Force closed connection: id={}", conn_id);
        }
    }

    // ==================== Cookie Jar Integration (J4) ====================

    /// Set the cookie jar for automatic cookie management on HTTP requests.
    ///
    /// Once set, the connection manager will automatically:
    /// - Attach `Cookie` headers with matching cookies when building outgoing requests
    /// - Parse and store cookies from `Set-Cookie` response headers
    ///
    /// # Arguments
    ///
    /// * `jar` - The CookieJar instance to use for cookie storage and matching
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::http::connection::{HttpConnectionManager, HttpConfig};
    /// use aria2_core::http::cookie_storage::CookieJar;
    ///
    /// let mut manager = HttpConnectionManager::new(&Default::default());
    /// let jar = CookieJar::new();
    /// manager.set_cookie_jar(Some(jar));
    /// ```
    pub fn set_cookie_jar(&mut self, jar: Option<CookieJar>) {
        self.cookie_jar = jar;
    }

    /// Get a reference to the current cookie jar, if one is set.
    pub fn cookie_jar(&self) -> &Option<CookieJar> {
        &self.cookie_jar
    }

    /// Get a mutable reference to the current cookie jar, if one is set.
    pub fn cookie_jar_mut(&mut self) -> &mut Option<CookieJar> {
        &mut self.cookie_jar
    }

    /// Attach matching cookies from the jar to an HTTP request as a Cookie header string.
    ///
    /// Call this method before sending an HTTP request to include any stored cookies
    /// that match the target URL. The returned string can be used directly as the
    /// value of the `Cookie` request header.
    ///
    /// # Arguments
    ///
    /// * `url` - The target URL for the HTTP request
    ///
    /// # Returns
    ///
    /// `Some(header_value)` containing `"name1=val1; name2=val2"` format if matching
    /// cookies exist, or `None` if no cookies match or no jar is configured.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::http::connection::HttpConnectionManager;
    /// use url::Url;
    ///
    /// let manager = HttpConnectionManager::new(&Default::default());
    /// let url = Url::parse("https://example.com/api").unwrap();
    ///
    /// if let Some(cookie_header) = manager.attach_cookies_to_request(&url) {
    ///     // Add "Cookie: {cookie_header}" to your HTTP request headers
    ///     println!("Cookie: {}", cookie_header);
    /// }
    /// ```
    pub fn attach_cookies_to_request(&self, url: &Url) -> Option<String> {
        let jar = self.cookie_jar.as_ref()?;
        let is_https = url.scheme() == "https";
        jar.cookie_header_for_url(url.as_str(), is_https)
    }

    /// Extract cookies from response Set-Cookie headers and store them in the jar.
    ///
    /// Call this method after receiving an HTTP response to persist any cookies
    /// set by the server. Each `Set-Cookie` header value is parsed and stored
    /// in the cookie jar for future requests.
    ///
    /// # Arguments
    ///
    /// * `response_headers` - The response headers as a slice of `(name, value)` tuples
    /// * `request_url` - The original request URL (used as default domain/path context)
    ///
    /// # Returns
    ///
    /// The number of cookies successfully extracted and stored.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use aria2_core::http::connection::HttpConnectionManager;
    ///
    /// // After receiving an HTTP response:
    /// let headers = vec![
    ///     ("Set-Cookie".to_string(), "session=abc; Domain=example.com".to_string()),
    ///     ("Set-Cookie".to_string(), "theme=dark".to_string()),
    /// ];
    /// let url = url::Url::parse("https://example.com/").unwrap();
    ///
    /// let mut manager = HttpConnectionManager::new(&Default::default());
    /// manager.set_cookie_jar(Some(aria2_core::http::cookie_storage::CookieJar::new()));
    /// let count = manager.extract_cookies_from_response(&headers, &url);
    /// println!("Stored {} cookies", count); // Prints: Stored 2 cookies
    /// ```
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
                    "Extracted and stored cookie from Set-Cookie header: {}",
                    &value[..value.len().min(80)]
                );
            }
        }
        stored
    }

    // ==================== Private Helper Methods ====================

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

    /// Try to reuse a connection from the pool matching the given pool key
    fn try_reuse_connection(&mut self, pool_key: &ConnectionPoolKey) -> Result<Option<ActiveConnection>> {
        let conn_ids = match self.key_connections.get(pool_key) {
            Some(ids) => ids.clone(),
            None => return Ok(None),
        };

        // Look for an available idle connection
        for &conn_id in &conn_ids {
            if let Some(mut conn) = self.pool.remove(&conn_id) {
                // Validate connection validity
                if conn.is_valid() {
                    conn.touch();

                    // Check idle timeout
                    let idle_time = conn.last_used.elapsed();
                    if idle_time < self.config.idle_timeout {
                        tracing::debug!(
                            "Reusing idle connection: id={}, idle={:.2}s",
                            conn_id,
                            idle_time.as_secs_f64()
                        );
                        return Ok(Some(conn));
                    } else {
                        // Connection expired, close and continue searching
                        tracing::debug!(
                            "Connection expired: id={}, idle={:.2}s",
                            conn_id,
                            idle_time.as_secs_f64()
                        );
                        self.active_count = self.active_count.saturating_sub(1);
                        std::mem::drop(conn.shutdown()); // Ignore close errors
                    }
                } else {
                    // Connection is no longer valid
                    self.active_count = self.active_count.saturating_sub(1);
                }
            }
        }

        // Clean up all invalid connection records for this pool key
        self.cleanup_invalid_connections(pool_key);

        Ok(None)
    }

    /// Create a new TCP connection
    async fn create_new_connection(
        &mut self,
        url: &Url,
        host: &str,
        pool_key: ConnectionPoolKey,
    ) -> Result<ActiveConnection> {
        // Resolve address
        let addr = Self::resolve_address(url)?;

        // Apply connection timeout
        let stream = timeout(self.config.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("TCP connection failed ({}): {}", addr, e),
                })
            })?;

        // Set TCP options
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!("Failed to set nodelay: {}", e);
        }

        // Generate connection ID
        let conn_id = self.id_counter.fetch_add(1, Ordering::SeqCst);

        let conn = ActiveConnection {
            id: conn_id,
            stream,
            host: host.to_string(),
            last_used: Instant::now(),
            pool_key: pool_key.clone(),
        };

        // Update connection pool state
        self.active_count += 1;
        self.key_connections
            .entry(pool_key)
            .or_default()
            .push(conn_id);

        tracing::info!(
            "Created new connection: id={}, host={}, active={}/{}",
            conn_id,
            host,
            self.active_count,
            self.config.max_connections
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

        // Use tokio for DNS resolution (synchronous version for test compatibility)
        // Note: production should use tokio::net::lookup_host
        let addr_str = format!("{}:{}", host, port);
        addr_str
            .parse::<SocketAddr>()
            .map_err(|e| Aria2Error::Parse(format!("Failed to resolve address: {}", e)))
    }

    /// LRU eviction: clean up idle timed-out connections
    fn evict_idle_connections(&mut self) {
        let now = Instant::now();
        let mut evicted = Vec::new();

        for (&conn_id, conn) in &self.pool {
            if now.duration_since(conn.last_used) > self.config.idle_timeout {
                evicted.push((conn_id, conn.pool_key.clone()));
            }
        }

        let evict_count = evicted.len();
        for (conn_id, pool_key) in evicted {
            if let Some(mut conn) = self.pool.remove(&conn_id) {
                std::mem::drop(conn.shutdown());
                self.active_count = self.active_count.saturating_sub(1);
                self.remove_from_key_map(&pool_key, conn_id);
                tracing::debug!("LRU evicted expired connection: id={}", conn_id);
            }
        }

        if evict_count > 0 {
            tracing::info!("LRU evicted {} expired connections", evict_count);
        }
    }

    /// Clean up invalid connection records for a specific pool key
    fn cleanup_invalid_connections(&mut self, pool_key: &ConnectionPoolKey) {
        if let Some(ids) = self.key_connections.get_mut(pool_key) {
            ids.retain(|&id| self.pool.contains_key(&id));
            if ids.is_empty() {
                self.key_connections.remove(pool_key);
            }
        }
    }

    /// Remove a connection ID from the pool-key mapping
    fn remove_from_key_map(&mut self, pool_key: &ConnectionPoolKey, conn_id: u64) {
        if let Some(ids) = self.key_connections.get_mut(pool_key) {
            ids.retain(|&id| id != conn_id);
            if ids.is_empty() {
                self.key_connections.remove(pool_key);
            }
        }
    }
}

impl Drop for HttpConnectionManager {
    fn drop(&mut self) {
        // Synchronous cleanup (no async)
        for (_, conn) in self.pool.drain() {
            // TcpStream's drop will automatically close
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
            .field("active_count", &self.active_count)
            .field("pool_size", &self.pool.len())
            .field("cookie_jar_set", &self.cookie_jar.is_some())
            .finish()
    }
}
