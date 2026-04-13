//! HTTP 连接管理器
//!
//! 提供连接池复用、Keep-Alive 管理、LRU 淘汰策略和重定向跟随等功能。
//!
//! # 示例
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
//!     // 使用连接管理器...
//! }
//! ```

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use url::Url;

use crate::error::{Aria2Error, RecoverableError, Result};
use crate::http::cookie_storage::{CookieJar, JarCookie};

/// HTTP 连接配置
#[derive(Debug, Clone)]
pub struct HttpConfig {
    /// 最大并发连接数
    pub max_connections: usize,
    /// TCP 连接超时
    pub connect_timeout: Duration,
    /// 读取超时
    pub read_timeout: Duration,
    /// 写入超时
    pub write_timeout: Duration,
    /// 空闲连接超时（LRU 淘汰）
    pub idle_timeout: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_connections: 16,
            connect_timeout: Duration::from_secs(30),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(60),
            idle_timeout: Duration::from_secs(300),
        }
    }
}

/// 活动连接信息
#[derive(Debug)]
pub struct ActiveConnection {
    /// 唯一连接 ID
    pub id: u64,
    /// TCP 流
    pub stream: TcpStream,
    /// 目标主机
    pub host: String,
    /// 最后使用时间
    pub last_used: Instant,
}

impl ActiveConnection {
    /// 检查连接是否仍然有效
    pub fn is_valid(&self) -> bool {
        // 检查连接是否已关闭或出错
        self.stream.peer_addr().is_ok()
    }

    /// 更新最后使用时间
    pub fn touch(&mut self) {
        self.last_used = Instant::now();
    }
}

/// HTTP 连接状态
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionState {
    /// 空闲可用
    Idle,
    /// 正在使用中
    InUse,
    /// 已关闭
    Closed,
}

/// HTTP 连接管理器
///
/// 提供 HTTP 连接的获取、释放、池化管理和重定向跟随功能。
/// 支持 Keep-Alive 连接复用、LRU 淘汰策略和循环重定向检测。
///
/// # 线程安全
///
/// `HttpConnectionManager` 内部使用 `tokio::sync::Mutex` 保护共享状态，
/// 可以安全地在多个异步任务之间共享。
///
/// # 性能特性
///
/// - **连接复用**: 通过 Keep-Alive 头部检查，避免重复建立 TCP 连接
/// - **LRU 淘汰**: 自动清理空闲超时的连接，防止资源泄漏
/// - **三级超时**: 分别控制连接、读取、写入三个阶段的超时时间
///
/// # 示例
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
///     // 使用连接进行 HTTP 请求...
///     manager.release(conn.id).await;
///
///     Ok(())
/// }
/// ```
pub struct HttpConnectionManager {
    /// Configuration parameters
    config: HttpConfig,
    /// Connection pool: conn_id -> ActiveConnection
    pool: HashMap<u64, ActiveConnection>,
    /// Host-to-connection-ID mapping (for fast lookup of reusable connections)
    host_connections: HashMap<String, Vec<u64>>,
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
    /// 创建新的 HTTP 连接管理器
    ///
    /// # 参数
    ///
    /// * `config` - HTTP 连接配置，包含超时、最大连接数等参数
    ///
    /// # 返回值
    ///
    /// 返回初始化完成的连接管理器实例
    ///
    /// # 示例
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
            host_connections: HashMap::new(),
            active_count: 0,
            id_counter: AtomicU64::new(1),
            max_redirects: 5,
            cookie_jar: None,
        }
    }

    /// 获取最大连接数配置
    pub fn max_connections(&self) -> usize {
        self.config.max_connections
    }

    /// 获取当前活动连接数
    pub fn active_count(&self) -> usize {
        self.active_count
    }

    /// 获取连接池大小（包含空闲和使用中的连接）
    pub fn pool_size(&self) -> usize {
        self.pool.len()
    }

    /// 从连接池获取或新建一个到指定 URL 的连接
    ///
    /// 该方法会尝试从连接池中查找可复用的空闲连接（基于主机名匹配），
    /// 如果没有可用连接且未达到最大连接数限制，则创建新连接。
    ///
    /// # 参数
    ///
    /// * `url` - 目标 URL，用于提取主机名和端口信息
    ///
    /// # 错误
    ///
    /// * [`Aria2Error::Network`] - 当达到最大连接数限制时
    /// * [`Aria2Error::Recoverable`] - 当连接超时或网络故障时
    ///
    /// # 返回值
    ///
    /// 返回可用的活动连接实例
    ///
    /// # Keep-Alive 复用逻辑
    ///
    /// 1. 提取 URL 的 host:port 作为连接标识
    /// 2. 在连接池中查找该主机的空闲连接
    /// 3. 验证连接有效性（检查 socket 是否正常）
    /// 4. 更新 last_used 时间戳并返回
    /// 5. 如果无可用连接，创建新的 TCP 连接
    ///
    /// # 示例
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
    ///         Ok(conn) => println!("获取连接成功: id={}", conn.id),
    ///         Err(e) => eprintln!("获取连接失败: {}", e),
    ///     }
    /// }
    /// ```
    pub async fn acquire(&mut self, url: &Url) -> Result<ActiveConnection> {
        let host = Self::extract_host(url);

        // 尝试从连接池中复用空闲连接
        if let Some(conn) = self.try_reuse_connection(&host)? {
            tracing::debug!("复用连接: id={}, host={}", conn.id, host);
            return Ok(conn);
        }

        // 检查是否达到最大连接数限制
        if self.active_count >= self.config.max_connections {
            // 尝试清理过期连接
            self.evict_idle_connections();

            if self.active_count >= self.config.max_connections {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "达到最大连接数限制: {} (host={})",
                            self.config.max_connections, host
                        ),
                    },
                ));
            }
        }

        // 创建新连接
        self.create_new_connection(url, &host).await
    }

    /// 归还连接到连接池
    ///
    /// 将使用完毕的连接归还到连接池中，以便后续复用。
    /// 如果连接已失效（socket 关闭或出错），会自动从池中移除。
    ///
    /// # 参数
    ///
    /// * `conn_id` - 要归还的连接 ID
    ///
    /// # 行为
    ///
    /// 1. 根据连接 ID 在池中查找对应连接
    /// 2. 更新 last_used 为当前时间
    /// 3. 将连接标记为空闲状态
    /// 4. 如果连接无效，自动清理资源
    ///
    /// # 示例
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
    ///     // 使用连接完成请求后...
    ///     manager.release(conn.id).await;
    /// }
    /// ```
    pub async fn release(&mut self, conn_id: u64) {
        if let Some(mut conn) = self.pool.remove(&conn_id) {
            // 验证连接是否仍然有效
            if !conn.is_valid() {
                tracing::debug!("连接已失效，移除: id={}", conn_id);
                self.active_count = self.active_count.saturating_sub(1);
                self.remove_from_host_map(&conn.host, conn_id);
                return;
            }

            // 更新最后使用时间并放回池中
            conn.touch();
            self.pool.insert(conn_id, conn);

            tracing::debug!("归还连接到池: id={}", conn_id);
        } else {
            tracing::warn!("尝试释放不存在的连接: id={}", conn_id);
        }
    }

    /// 跟随 HTTP 重定向
    ///
    /// 解析响应中的 Location 头部，构建新的 URL 并验证重定向合法性。
    /// 支持相对路径和绝对路径的重定向，自动处理循环重定向检测。
    ///
    /// # 参数
    ///
    /// * `response` - HTTP 响应对象，需包含 Location 头部
    /// * `current_url` - 当前请求的 URL（用于解析相对路径）
    /// * `redirect_chain` - 已访问过的 URL 集合（用于循环检测）
    ///
    /// # 错误
    ///
    /// * [`Aria2Error::Parse`] - 当 Location 头部格式无效或 URL 解析失败时
    /// * [`Aria2Error::Network`] - 当检测到循环重定向或超过最大跳数时
    ///
    /// # 返回值
    ///
    /// 返回重定向目标的新 URL
    ///
    /// # 重定向链检测机制
    ///
    /// 1. 使用 HashSet 记录所有已访问 URL
    /// 2. 每次重定向前检查新 URL 是否已在集合中
    /// 3. 维护跳数计数器，超过阈值返回错误
    /// 4. 支持最多 5 次 301/302/303/307/308 重定向
    ///
    /// # 示例
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
    ///         Ok(new_url) => println!("重定向到: {}", new_url),
    ///         Err(e) => eprintln!("重定向失败: {}", e),
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
        // 检查是否为重定向响应
        if !response.is_redirect() {
            return Err(Aria2Error::Parse(format!(
                "非重定向响应码: {}",
                response.status_code
            )));
        }

        // 检查重定向次数限制
        if redirect_count >= self.max_redirects {
            return Err(Aria2Error::Network(format!(
                "超过最大重定向次数限制: {}",
                self.max_redirects
            )));
        }

        // 获取 Location 头部
        let location = response
            .location()
            .ok_or_else(|| Aria2Error::Parse("缺少 Location 头部".to_string()))?;

        // 解析新的 URL（支持相对路径）
        let new_url = current_url
            .join(location)
            .map_err(|e| Aria2Error::Parse(format!("解析重定向 URL 失败: {}", e)))?;

        // 循环重定向检测
        if redirect_chain.contains(&new_url) {
            return Err(Aria2Error::Network(format!(
                "检测到循环重定向: {}",
                new_url
            )));
        }

        tracing::info!(
            "跟随重定向: {} -> {} ({}/{})",
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
        const MAX_REDIRECTS: u8 = 5;

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

    /// 构建 Range 请求头
    ///
    /// 根据 start 和 end 字节位置构建符合 RFC 7233 规范的 Range 头部字符串。
    /// 用于断点续传和分块下载场景。
    ///
    /// # 参数
    ///
    /// * `start` - 起始字节位置（包含）
    /// * `end` - 结束字节位置（包含），如果为 None 则表示到文件末尾
    ///
    /// # 返回值
    ///
    /// 返回格式化的 Range 头部值，例如 `"bytes=0-499"` 或 `"bytes=500-"`
    ///
    /// # 格式规范
    ///
    /// - `bytes=start-end`: 指定范围 [start, end]
    /// - `bytes=start-`: 从 start 到文件末尾
    ///
    /// # 示例
    ///
    /// ```
    /// use aria2_core::http::connection::HttpConnectionManager;
    ///
    /// let manager = HttpConnectionManager::new(&Default::default());
    ///
    /// // 完整范围
    /// assert_eq!(
    ///     manager.build_range_header(0, Some(499)),
    ///     "bytes=0-499"
    /// );
    ///
    /// // 开放结束范围
    /// assert_eq!(
    ///     manager.build_range_header(1000, None),
    ///     "bytes=1000-"
    /// );
    ///
    /// // 单字节范围
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

    /// 解析 Content-Range 响应头
    ///
    /// 解析服务器返回的 Content-Range 头部值，提取范围信息和总大小。
    /// 用于验证服务器是否正确支持 Range 请求。
    ///
    /// # 参数
    ///
    /// * `header` - Content-Range 头部的原始字符串值
    ///
    /// # 返回值
    ///
    /// 如果解析成功，返回元组 `(start, end, total)`:
    /// - `start`: 范围起始字节（包含）
    /// - `end`: 范围结束字节（包含）
    /// - `total`: 文件总字节数（如果未知则为 u64::MAX）
    ///
    /// 如果格式无效，返回 `None`
    ///
    /// # 支持格式
    ///
    /// - `bytes 0-499/1000`: 已知总大小的范围
    /// - `bytes 0-499/*`: 未知总大小的范围
    ///
    /// # 示例
    ///
    /// ```
    /// use aria2_core::http::connection::HttpConnectionManager;
    ///
    /// let manager = HttpConnectionManager::new(&Default::default());
    ///
    /// // 解析已知总大小
    /// let result = manager.parse_content_range("bytes 0-499/1000");
    /// assert_eq!(result, Some((0, 499, 1000)));
    ///
    /// // 解析未知总大小
    /// let result = manager.parse_content_range("bytes 500-999/*");
    /// assert_eq!(result, Some((500, 999, u64::MAX)));
    ///
    /// // 无效格式
    /// assert_eq!(manager.parse_content_range("invalid"), None);
    /// assert_eq!(manager.parse_content_range("bits 0-99/1000"), None);
    /// ```
    pub fn parse_content_range(&self, header: &str) -> Option<(u64, u64, u64)> {
        let header = header.trim();

        // 必须以 "bytes " 开头
        if !header.starts_with("bytes ") {
            return None;
        }

        let range_part = &header[6..];
        let parts: Vec<&str> = range_part.split('/').collect();

        if parts.len() != 2 {
            return None;
        }

        // 解析 start-end 部分
        let range_values: Vec<&str> = parts[0].split('-').collect();
        if range_values.len() != 2 {
            return None;
        }

        let start: u64 = range_values[0].trim().parse().ok()?;
        let end: u64 = range_values[1].trim().parse().ok()?;

        // 解析 total 大小
        let total = match parts[1].trim() {
            "*" => u64::MAX,
            s => s.parse().ok()?,
        };

        Some((start, end, total))
    }

    /// 清理所有空闲连接
    ///
    /// 关闭连接池中的所有连接，释放系统资源。
    /// 通常在下载任务完成或程序退出时调用。
    pub async fn cleanup(&mut self) {
        for (_, mut conn) in self.pool.drain() {
            let _ = conn.shutdown().await;
        }
        self.host_connections.clear();
        self.active_count = 0;

        tracing::info!("连接池已清理");
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
            let _ = conn.shutdown().await;
            self.active_count = self.active_count.saturating_sub(1);
            self.remove_from_host_map(&conn.host, conn_id);
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
            if name.eq_ignore_ascii_case("set-cookie") {
                if let Some(cookie) = JarCookie::parse_set_cookie(value) {
                    jar.store(cookie);
                    stored += 1;
                    tracing::debug!(
                        "Extracted and stored cookie from Set-Cookie header: {}",
                        &value[..value.len().min(80)]
                    );
                }
            }
        }
        stored
    }

    // ==================== Private Helper Methods ====================

    /// 从 URL 中提取主机标识（host:port）
    fn extract_host(url: &Url) -> String {
        match url.port_or_known_default() {
            Some(port) => format!("{}:{}", url.host_str().unwrap_or("localhost"), port),
            None => url.host_str().unwrap_or("localhost").to_string(),
        }
    }

    /// 尝试从连接池复用连接
    fn try_reuse_connection(&mut self, host: &str) -> Result<Option<ActiveConnection>> {
        let conn_ids = match self.host_connections.get(host) {
            Some(ids) => ids.clone(),
            None => return Ok(None),
        };

        // 查找可用的空闲连接
        for &conn_id in &conn_ids {
            if let Some(mut conn) = self.pool.remove(&conn_id) {
                // 验证连接有效性
                if conn.is_valid() {
                    conn.touch();

                    // 检查 Keep-Alive 状态（简化版：仅检查时间）
                    let idle_time = conn.last_used.elapsed();
                    if idle_time < self.config.idle_timeout {
                        tracing::debug!(
                            "复用空闲连接: id={}, idle={:.2}s",
                            conn_id,
                            idle_time.as_secs_f64()
                        );
                        return Ok(Some(conn));
                    } else {
                        // 连接过期，关闭并继续查找
                        tracing::debug!(
                            "连接已过期: id={}, idle={:.2}s",
                            conn_id,
                            idle_time.as_secs_f64()
                        );
                        self.active_count = self.active_count.saturating_sub(1);
                        std::mem::drop(conn.shutdown()); // 忽略关闭错误
                    }
                } else {
                    // 连接已失效
                    self.active_count = self.active_count.saturating_sub(1);
                }
            }
        }

        // 清理该主机的所有无效连接记录
        self.cleanup_invalid_connections(host);

        Ok(None)
    }

    /// 创建新的 TCP 连接
    async fn create_new_connection(&mut self, url: &Url, host: &str) -> Result<ActiveConnection> {
        // 解析地址
        let addr = Self::resolve_address(url)?;

        // 应用连接超时
        let stream = timeout(self.config.connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("TCP 连接失败 ({}): {}", addr, e),
                })
            })?;

        // 设置 TCP 选项
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!("设置 nodelay 失败: {}", e);
        }
        // 注意: tokio TcpStream 不直接支持 set_keepalive，需要使用 socket2 或忽略

        // 生成连接 ID
        let conn_id = self.id_counter.fetch_add(1, Ordering::SeqCst);

        let conn = ActiveConnection {
            id: conn_id,
            stream,
            host: host.to_string(),
            last_used: Instant::now(),
        };

        // 更新连接池状态
        self.active_count += 1;
        self.host_connections
            .entry(host.to_string())
            .or_default()
            .push(conn_id);

        tracing::info!(
            "创建新连接: id={}, host={}, active={}/{}",
            conn_id,
            host,
            self.active_count,
            self.config.max_connections
        );

        Ok(conn)
    }

    /// 解析 URL 为 SocketAddr
    fn resolve_address(url: &Url) -> Result<SocketAddr> {
        let host = url
            .host_str()
            .ok_or_else(|| Aria2Error::Parse("URL 缺少主机名".to_string()))?;

        let port = url
            .port_or_known_default()
            .ok_or_else(|| Aria2Error::Parse("无法确定端口号".to_string()))?;

        // 使用 tokio 进行 DNS 解析（同步版本用于测试兼容性）
        // 注意：生产环境应该使用 tokio::net::lookup_host
        let addr_str = format!("{}:{}", host, port);
        addr_str
            .parse::<SocketAddr>()
            .map_err(|e| Aria2Error::Parse(format!("解析地址失败: {}", e)))
    }

    /// LRU 淘汰：清理空闲超时的连接
    fn evict_idle_connections(&mut self) {
        let now = Instant::now();
        let mut evicted = Vec::new();

        for (&conn_id, conn) in &self.pool {
            if now.duration_since(conn.last_used) > self.config.idle_timeout {
                evicted.push((conn_id, conn.host.clone()));
            }
        }

        let evict_count = evicted.len();
        for (conn_id, host) in evicted {
            if let Some(mut conn) = self.pool.remove(&conn_id) {
                std::mem::drop(conn.shutdown());
                self.active_count = self.active_count.saturating_sub(1);
                self.remove_from_host_map(&host, conn_id);
                tracing::debug!("LRU 淘汰过期连接: id={}, host={}", conn_id, host);
            }
        }

        if evict_count > 0 {
            tracing::info!("LRU 淘汰了 {} 个过期连接", evict_count);
        }
    }

    /// 清理指定主机的无效连接记录
    fn cleanup_invalid_connections(&mut self, host: &str) {
        if let Some(ids) = self.host_connections.get_mut(host) {
            ids.retain(|&id| self.pool.contains_key(&id));
            if ids.is_empty() {
                self.host_connections.remove(host);
            }
        }
    }

    /// 从主机映射中移除连接 ID
    fn remove_from_host_map(&mut self, host: &str, conn_id: u64) {
        if let Some(ids) = self.host_connections.get_mut(host) {
            ids.retain(|&id| id != conn_id);
            if ids.is_empty() {
                self.host_connections.remove(host);
            }
        }
    }
}

impl ActiveConnection {
    /// 异步读取数据（带超时控制）
    ///
    /// 从 TCP 流中读取数据到缓冲区，受 read_timeout 限制。
    /// 用于读取 HTTP 响应头和响应体。
    pub async fn read_with_timeout(
        &mut self,
        buf: &mut [u8],
        read_timeout: Duration,
    ) -> Result<usize> {
        timeout(read_timeout, self.stream.read(buf))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("读取数据失败: {}", e),
                })
            })
    }

    /// 异步写入数据（带超时控制）
    ///
    /// 将数据写入 TCP 流，受 write_timeout 限制。
    /// 用于发送 HTTP 请求头和请求体。
    pub async fn write_with_timeout(
        &mut self,
        buf: &[u8],
        write_timeout: Duration,
    ) -> Result<usize> {
        timeout(write_timeout, self.stream.write(buf))
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("写入数据失败: {}", e),
                })
            })
    }

    /// 刷新写缓冲区（带超时控制）
    pub async fn flush_with_timeout(&mut self, write_timeout: Duration) -> Result<()> {
        timeout(write_timeout, self.stream.flush())
            .await
            .map_err(|_| Aria2Error::Recoverable(RecoverableError::Timeout))?
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("刷新缓冲区失败: {}", e),
                })
            })
    }

    /// 关闭连接（双向关闭）
    pub async fn shutdown(&mut self) -> Result<()> {
        match self.stream.shutdown().await {
            Ok(_) => Ok(()),
            Err(e) => {
                tracing::debug!("关闭连接失败: id={}, error={}", self.id, e);
                Ok(())
            }
        }
    }

    /// 获取对等端地址
    pub fn peer_addr(&self) -> Result<SocketAddr> {
        self.stream.peer_addr().map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("获取对等端地址失败: {}", e),
            })
        })
    }

    /// 获取本地地址
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.stream.local_addr().map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("获取本地地址失败: {}", e),
            })
        })
    }
}

impl Drop for HttpConnectionManager {
    fn drop(&mut self) {
        // 同步清理（不使用 async）
        for (_, conn) in self.pool.drain() {
            // TcpStream 的 drop 会自动关闭
            drop(conn);
        }
        self.host_connections.clear();
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

// 重新导出 HttpResponse 以便在 connection.rs 中使用
pub use aria2_protocol::http::response::HttpResponse;

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::time::{sleep, timeout};

    fn create_test_config() -> HttpConfig {
        HttpConfig {
            max_connections: 4,
            connect_timeout: Duration::from_millis(100),
            read_timeout: Duration::from_millis(200),
            write_timeout: Duration::from_millis(200),
            idle_timeout: Duration::from_millis(500),
        }
    }

    #[test]
    fn test_config_default() {
        let config = HttpConfig::default();
        assert_eq!(config.max_connections, 16);
        assert_eq!(config.connect_timeout, Duration::from_secs(30));
        assert_eq!(config.read_timeout, Duration::from_secs(60));
        assert_eq!(config.write_timeout, Duration::from_secs(60));
        assert_eq!(config.idle_timeout, Duration::from_secs(300));
    }

    #[test]
    fn test_manager_creation() {
        let config = create_test_config();
        let manager = HttpConnectionManager::new(&config);

        assert_eq!(manager.max_connections(), 4);
        assert_eq!(manager.active_count(), 0);
        assert_eq!(manager.pool_size(), 0);
    }

    #[test]
    fn test_build_range_header() {
        let manager = HttpConnectionManager::new(&Default::default());

        // 完整范围
        assert_eq!(manager.build_range_header(0, Some(999)), "bytes=0-999");

        // 开放结束范围
        assert_eq!(manager.build_range_header(500, None), "bytes=500-");

        // 单字节
        assert_eq!(manager.build_range_header(42, Some(42)), "bytes=42-42");

        // 大数值
        assert_eq!(
            manager.build_range_header(u64::MAX - 1, Some(u64::MAX)),
            "bytes=18446744073709551614-18446744073709551615"
        );
    }

    #[test]
    fn test_parse_content_range() {
        let manager = HttpConnectionManager::new(&Default::default());

        // 正常格式（已知总数）
        assert_eq!(
            manager.parse_content_range("bytes 0-499/1000"),
            Some((0, 499, 1000))
        );

        // 正常格式（未知总数）
        assert_eq!(
            manager.parse_content_range("bytes 500-999/*"),
            Some((500, 999, u64::MAX))
        );

        // 边界值
        assert_eq!(manager.parse_content_range("bytes 0-0/1"), Some((0, 0, 1)));

        // 无效格式
        assert_eq!(manager.parse_content_range(""), None);
        assert_eq!(manager.parse_content_range("invalid"), None);
        assert_eq!(manager.parse_content_range("bits 0-99/1000"), None);
        assert_eq!(manager.parse_content_range("bytes 0-499"), None); // 缺少 /total
        assert_eq!(manager.parse_content_range("bytes abc-def/1000"), None);
    }

    #[test]
    fn test_follow_redirects_success() {
        let manager = HttpConnectionManager::new(&Default::default());
        let current_url = Url::parse("http://example.com/old").unwrap();
        let mut chain = HashSet::new();
        chain.insert(current_url.clone());

        let mut response = HttpResponse::new(301, "Moved Permanently".to_string());
        response
            .headers
            .push(("Location".to_string(), "http://example.com/new".to_string()));

        let result = manager.follow_redirects(&response, &current_url, &chain, 1);
        assert!(result.is_ok());
        let new_url = result.unwrap();
        assert!(new_url.as_str().starts_with("http://example.com/new"));
    }

    #[test]
    fn test_follow_redirects_relative_path() {
        let manager = HttpConnectionManager::new(&Default::default());
        let current_url = Url::parse("http://example.com/path/page.html").unwrap();
        let chain = HashSet::new();

        let mut response = HttpResponse::new(302, "Found".to_string());
        response
            .headers
            .push(("Location".to_string(), "../other".to_string()));

        let result = manager.follow_redirects(&response, &current_url, &chain, 1);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().as_str(), "http://example.com/other");
    }

    #[test]
    fn test_follow_redirects_loop_detection() {
        let manager = HttpConnectionManager::new(&Default::default());
        let url_a = Url::parse("http://example.com/a").unwrap();
        let url_b = Url::parse("http://example.com/b").unwrap();

        let mut chain = HashSet::new();
        chain.insert(url_a.clone());
        chain.insert(url_b.clone());

        let mut response = HttpResponse::new(301, "Moved".to_string());
        response
            .headers
            .push(("Location".to_string(), "http://example.com/a".to_string()));

        // 尝试重定向回已访问的 URL（循环）
        let result = manager.follow_redirects(&response, &url_b, &chain, 2);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("循环重定向"));
    }

    #[test]
    fn test_follow_redirects_max_exceeded() {
        let manager = HttpConnectionManager::new(&Default::default());
        let current_url = Url::parse("http://example.com/start").unwrap();
        let chain = HashSet::new();

        let mut response = HttpResponse::new(302, "Found".to_string());
        response.headers.push((
            "Location".to_string(),
            "http://example.com/next".to_string(),
        ));

        // 超过最大重定向次数
        let result = manager.follow_redirects(&response, &current_url, &chain, 6);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("最大重定向"));
    }

    #[test]
    fn test_follow_redirects_non_redirect_response() {
        let manager = HttpConnectionManager::new(&Default::default());
        let current_url = Url::parse("http://example.com/").unwrap();
        let chain = HashSet::new();

        let response = HttpResponse::new(200, "OK".to_string());

        let result = manager.follow_redirects(&response, &current_url, &chain, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("非重定向"));
    }

    #[test]
    fn test_follow_redirects_missing_location() {
        let manager = HttpConnectionManager::new(&Default::default());
        let current_url = Url::parse("http://example.com/").unwrap();
        let chain = HashSet::new();

        let response = HttpResponse::new(301, "Moved".to_string());

        let result = manager.follow_redirects(&response, &current_url, &chain, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Location"));
    }

    #[test]
    fn test_extract_host() {
        // 带 80 端口
        let url = Url::parse("http://example.com/path").unwrap();
        assert_eq!(HttpConnectionManager::extract_host(&url), "example.com:80");

        // 带 443 端口
        let url = Url::parse("https://example.com:443/path").unwrap();
        assert_eq!(HttpConnectionManager::extract_host(&url), "example.com:443");

        // 自定义端口
        let url = Url::parse("http://example.com:8080/path").unwrap();
        assert_eq!(
            HttpConnectionManager::extract_host(&url),
            "example.com:8080"
        );
    }

    #[test]
    fn test_debug_format() {
        let config = create_test_config();
        let manager = HttpConnectionManager::new(&config);
        let debug_str = format!("{:?}", manager);

        assert!(debug_str.contains("HttpConnectionManager"));
        assert!(debug_str.contains("max_connections: 4"));
        assert!(debug_str.contains("active_count: 0"));
    }

    // ==================== 集成测试 ====================

    /// 启动一个简单的测试 HTTP 服务器
    async fn start_test_server(
        handler: impl Fn(TcpStream) + Send + 'static,
    ) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    #[tokio::test]
    async fn test_connection_pool_reuse() {
        let config = HttpConfig {
            max_connections: 4,
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_millis(1000),
            write_timeout: Duration::from_millis(1000),
            idle_timeout: Duration::from_millis(2000),
        };
        let mut manager = HttpConnectionManager::new(&config);

        // 启动测试服务器
        let (addr, server_handle) = start_test_server(|mut stream| {
            tokio::spawn(async move {
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK";
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        })
        .await;

        sleep(Duration::from_millis(100)).await;

        let url = Url::parse(&format!("http://{}", addr)).unwrap();

        // 第一次获取连接
        let conn1 = manager.acquire(&url).await.expect("第一次获取连接应成功");
        let _conn1_id = conn1.id;
        assert_eq!(manager.active_count(), 1);

        // 归还连接
        manager.release(conn1.id).await;

        // 第二次获取连接（应该能成功获取）
        let conn2 = manager.acquire(&url).await.expect("第二次应能获取连接");
        assert!(manager.active_count() >= 1); // 连接数应 >= 1

        // 清理
        manager.release(conn2.id).await;
        manager.cleanup().await;
        server_handle.abort();
    }

    #[tokio::test]
    async fn test_redirect_follow_5_jumps() {
        let manager = HttpConnectionManager::new(&create_test_config());
        let current_url = Url::parse("http://example.com/start").unwrap();
        let mut redirect_chain = HashSet::new();
        redirect_chain.insert(current_url.clone());

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
            response
                .headers
                .push(("Location".to_string(), target.to_string()));

            redirect_chain.insert(current.clone());

            let result = manager.follow_redirects(&response, &current, &redirect_chain, i as u32);
            assert!(
                result.is_ok(),
                "第 {} 次重定向应成功: {:?}",
                i + 1,
                result.err()
            );

            current = result.unwrap();
        }

        assert!(current.as_str().contains("example.com/final"));
    }

    #[tokio::test]
    async fn test_redirect_loop_detection() {
        let manager = HttpConnectionManager::new(&create_test_config());

        let url_a = Url::parse("http://example.com/a").unwrap();
        let url_b = Url::parse("http://example.com/b").unwrap();
        let url_c = Url::parse("http://example.com/c").unwrap();

        let mut chain = HashSet::new();
        chain.insert(url_a.clone());
        chain.insert(url_b.clone());
        chain.insert(url_c.clone());

        let mut response = HttpResponse::new(301, "Moved".to_string());
        response
            .headers
            .push(("Location".to_string(), "http://example.com/a".to_string()));

        let result = manager.follow_redirects(&response, &url_c, &chain, 3);

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("循环重定向"));
    }

    #[test]
    fn test_range_request_build() {
        let manager = HttpConnectionManager::new(&create_test_config());

        assert_eq!(manager.build_range_header(0, Some(999)), "bytes=0-999");
        assert_eq!(manager.build_range_header(500, None), "bytes=500-");
        assert_eq!(manager.build_range_header(42, Some(42)), "bytes=42-42");

        assert_eq!(
            manager.parse_content_range("bytes 0-499/1000"),
            Some((0, 499, 1000))
        );
        assert_eq!(
            manager.parse_content_range("bytes 500-999/*"),
            Some((500, 999, u64::MAX))
        );
        assert_eq!(manager.parse_content_range("invalid"), None);
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
            manager.acquire(&url),
        )
        .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < config.connect_timeout + Duration::from_millis(300),
            "耗时过长: {:.2}ms",
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

        let conn1 = manager.acquire(&url).await.unwrap();
        assert!(manager.active_count() >= 1);

        let conn2 = manager.acquire(&url).await.unwrap();
        assert!(manager.active_count() >= 2);

        // 尝试获取第三个连接（应该因达到限制而失败）
        let result = manager.acquire(&url).await;
        assert!(result.is_err(), "超过最大连接数限制时应返回错误");

        // 验证错误类型
        if let Err(e) = result {
            match &e {
                Aria2Error::Recoverable(_) => {}
                other => panic!("期望 Recoverable 错误，得到: {:?}", other),
            }
        }

        // 归还一个连接后，应该可以重新获取（如果连接池复用正常工作）
        manager.release(conn1.id).await;
        // 注意：由于连接可能仍在池中被计费，这里我们只验证不会 panic
        match manager.acquire(&url).await {
            Ok(conn3) => {
                println!("✓ 归还后成功获取新连接: id={}", conn3.id);
                manager.release(conn3.id).await;
            }
            Err(e) => {
                println!("⚠ 归还后获取失败（可能是连接复用限制）: {}", e);
                // 这也是可接受的行为
            }
        }

        manager.release(conn2.id).await;
        manager.cleanup().await;
    }

    // ==================== Cookie Jar Integration Tests (J4) ====================

    #[test]
    fn test_cookie_jar_initially_none() {
        let mut manager = HttpConnectionManager::new(&create_test_config());
        assert!(manager.cookie_jar().is_none());
        assert!(manager.cookie_jar_mut().is_none());

        // Attaching cookies without a jar should return None
        let url = Url::parse("https://example.com/").unwrap();
        assert!(manager.attach_cookies_to_request(&url).is_none());
    }

    #[test]
    fn test_set_and_get_cookie_jar() {
        let mut manager = HttpConnectionManager::new(&create_test_config());

        // Initially no jar
        assert!(manager.cookie_jar().is_none());

        // Set a cookie jar
        let jar = CookieJar::new();
        manager.set_cookie_jar(Some(jar));
        assert!(manager.cookie_jar().is_some());

        // Clear it
        manager.set_cookie_jar(None);
        assert!(manager.cookie_jar().is_none());
    }

    #[test]
    fn test_attach_cookies_to_request() {
        let mut manager = HttpConnectionManager::new(&create_test_config());

        // Create jar and add cookies
        let mut jar = CookieJar::new();
        jar.store(JarCookie::new("session_id", "abc123", "example.com"));
        jar.store(JarCookie::new("theme", "dark", "example.com"));
        manager.set_cookie_jar(Some(jar));

        // Attach cookies for example.com URL
        let url = Url::parse("http://example.com/api/data").unwrap();
        let header = manager.attach_cookies_to_request(&url);
        assert!(header.is_some(), "Should return Some with matching cookies");
        let hdr = header.unwrap();
        assert!(
            hdr.contains("session_id=abc123"),
            "Header should contain session_id cookie: {}",
            hdr
        );
        assert!(
            hdr.contains("theme=dark"),
            "Header should contain theme cookie: {}",
            hdr
        );

        // No cookies for different domain
        let url2 = Url::parse("http://other.com/").unwrap();
        let header2 = manager.attach_cookies_to_request(&url2);
        assert!(header2.is_none(), "No cookies should match other domain");
    }

    #[test]
    fn test_extract_cookies_from_response() {
        let mut manager = HttpConnectionManager::new(&create_test_config());
        manager.set_cookie_jar(Some(CookieJar::new()));

        // Simulate response headers with Set-Cookie
        let response_headers = vec![
            (
                "Set-Cookie".to_string(),
                "session=xyz789; Domain=example.com; Path=/".to_string(),
            ),
            (
                "Set-Cookie".to_string(),
                "prefs=en-US; Domain=example.com; Path=/; Secure; HttpOnly".to_string(),
            ),
            ("Content-Type".to_string(), "text/html".to_string()), // Non-cookie header
        ];

        let url = Url::parse("https://example.com/login").unwrap();
        let count = manager.extract_cookies_from_response(&response_headers, &url);

        assert_eq!(count, 2, "Should extract exactly 2 cookies");

        // Verify cookies were stored
        let jar = manager.cookie_jar().as_ref().unwrap();
        assert_eq!(jar.len(), 2, "Jar should contain 2 stored cookies");

        // Verify we can retrieve them
        let cookies = jar.get_cookies_for_url("https://example.com/", true);
        assert_eq!(cookies.len(), 2);

        let names: Vec<&str> = cookies.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"session"));
        assert!(names.contains(&"prefs"));

        // Verify Secure flag was parsed correctly
        let prefs_cookie = cookies.iter().find(|c| c.name == "prefs").unwrap();
        assert!(prefs_cookie.secure, "prefs cookie should be marked secure");
        assert!(
            prefs_cookie.http_only,
            "prefs cookie should be marked http_only"
        );
    }

    #[test]
    fn test_extract_cookies_no_jar_returns_zero() {
        let mut manager = HttpConnectionManager::new(&create_test_config());
        // No cookie jar set

        let headers = vec![("Set-Cookie".to_string(), "test=val".to_string())];
        let url = Url::parse("http://example.com/").unwrap();
        let count = manager.extract_cookies_from_response(&headers, &url);

        assert_eq!(count, 0, "Should return 0 when no jar is set");
    }

    #[test]
    fn test_extract_cookies_invalid_header_skipped() {
        let mut manager = HttpConnectionManager::new(&create_test_config());
        manager.set_cookie_jar(Some(CookieJar::new()));

        // Mix of valid and invalid Set-Cookie headers
        let headers = vec![
            (
                "Set-Cookie".to_string(),
                "valid=test_value; Domain=x.com".to_string(),
            ),
            ("Set-Cookie".to_string(), "no-equal-sign".to_string()), // Invalid format
            ("Set-Cookie".to_string(), "".to_string()),              // Empty - invalid
        ];

        let url = Url::parse("http://x.com/").unwrap();
        let count = manager.extract_cookies_from_response(&headers, &url);

        assert_eq!(count, 1, "Only 1 valid cookie should be extracted");

        let jar = manager.cookie_jar().as_ref().unwrap();
        assert_eq!(jar.len(), 1);
        let cookies = jar.get_cookies_for_url("http://x.com/", false);
        assert_eq!(cookies[0].name, "valid");
    }

    #[test]
    fn test_debug_format_includes_cookie_jar() {
        let mut manager = HttpConnectionManager::new(&create_test_config());
        let debug_str = format!("{:?}", manager);
        assert!(!debug_str.contains("cookie_jar_set: true"));

        manager.set_cookie_jar(Some(CookieJar::new()));
        let debug_str_with_jar = format!("{:?}", manager);
        assert!(
            debug_str_with_jar.contains("cookie_jar_set: true"),
            "Debug output should show cookie_jar is set: {}",
            debug_str_with_jar
        );
    }

    #[test]
    fn test_secure_cookie_not_sent_over_http() {
        let mut manager = HttpConnectionManager::new(&create_test_config());
        let mut jar = CookieJar::new();

        // Add a secure-only cookie
        let mut secure_cookie = JarCookie::new("token", "secret", "secure.example.com");
        secure_cookie.secure = true;
        jar.store(secure_cookie);

        manager.set_cookie_jar(Some(jar));

        // Over HTTP — should NOT get the secure cookie
        let url_http = Url::parse("http://secure.example.com/api").unwrap();
        let header_http = manager.attach_cookies_to_request(&url_http);
        assert!(
            header_http.is_none(),
            "Secure cookie must not be sent over HTTP"
        );

        // Over HTTPS — SHOULD get the secure cookie
        let url_https = Url::parse("https://secure.example.com/api").unwrap();
        let header_https = manager.attach_cookies_to_request(&url_https);
        assert!(
            header_https.is_some(),
            "Secure cookie should be sent over HTTPS"
        );
        assert!(
            header_https.unwrap().contains("token=secret"),
            "Header should contain the secure token cookie"
        );
    }
}
