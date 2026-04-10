//! HTTP 请求构建和响应解析模块
//!
//! 提供了完整的 HTTP/1.1 请求构建、响应解析、Cookie 管理和认证功能。
//! 支持流式 API 构建 HTTP 请求，自动添加标准 headers，以及 RFC 6265 合规的 Cookie 管理。

use base64::{engine::general_purpose, Engine};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use url::Url;

use crate::error::{Aria2Error, Result};

/// HTTP 请求方法枚举
#[derive(Debug, Clone, PartialEq)]
pub enum HttpMethod {
    /// GET 请求方法
    Get,
    /// POST 请求方法
    Post,
    /// HEAD 请求方法
    Head,
    /// PUT 请求方法
    Put,
    /// DELETE 请求方法
    Delete,
}

impl std::fmt::Display for HttpMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpMethod::Get => write!(f, "GET"),
            HttpMethod::Post => write!(f, "POST"),
            HttpMethod::Head => write!(f, "HEAD"),
            HttpMethod::Put => write!(f, "PUT"),
            HttpMethod::Delete => write!(f, "DELETE"),
        }
    }
}

/// HTTP 请求结构体
///
/// 表示一个完整的 HTTP/1.1 请求，包含方法、URL、headers 和可选的 body。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    /// HTTP 请求方法
    pub method: HttpMethod,
    /// 请求 URL
    pub url: Url,
    /// 请求 headers (支持多值)
    pub headers: HashMap<String, String>,
    /// 可选的请求体
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// 将 HTTP 请求序列化为原始字节
    ///
    /// 按照 HTTP/1.1 规范将请求序列化为可发送的字节序列。
    /// 格式: `METHOD PATH VERSION\r\nHeaders\r\n\r\nBody`
    ///
    /// # Returns
    ///
    /// 序列化后的字节数组
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut result = String::new();

        // 请求行: METHOD /path HTTP/1.1
        let path = self.url.path();
        let query = self.url.query();
        if let Some(q) = query {
            result.push_str(&format!("{} {}?{} HTTP/1.1\r\n", self.method, path, q));
        } else {
            result.push_str(&format!("{} {} HTTP/1.1\r\n", self.method, path));
        }

        // Headers
        for (key, value) in &self.headers {
            result.push_str(&format!("{}: {}\r\n", key, value));
        }

        // 空行分隔 header 和 body
        result.push_str("\r\n");

        let mut bytes = result.into_bytes();

        // Body
        if let Some(ref body) = self.body {
            bytes.extend_from_slice(body);
        }

        bytes
    }
}

/// HTTP 请求构建器 (Fluent API)
///
/// 使用流式 API 构建完整的 HTTP 请求，自动添加标准 headers。
///
/// # Examples
///
/// ```rust
/// use url::Url;
/// use aria2_core::http::request_response::{HttpRequestBuilder, HttpMethod};
///
/// let url = Url::parse("http://example.com/api").unwrap();
/// let request = HttpRequestBuilder::new(HttpMethod::Get, url)
///     .header("Accept", "application/json")
///     .build()
///     .unwrap();
/// ```
pub struct HttpRequestBuilder {
    /// HTTP 方法
    method: HttpMethod,
    /// 目标 URL
    url: Url,
    /// 自定义 headers
    headers: HashMap<String, String>,
    /// 可选的请求体
    body: Option<Vec<u8>>,
}

impl HttpRequestBuilder {
    /// 创建新的 HTTP 请求构建器
    ///
    /// # Arguments
    ///
    /// * `method` - HTTP 请求方法 (GET/POST/HEAD/PUT/DELETE)
    /// * `url` - 目标 URL
    ///
    /// # Returns
    ///
    /// 新的 HttpRequestBuilder 实例
    pub fn new(method: HttpMethod, url: Url) -> Self {
        Self {
            method,
            url,
            headers: HashMap::new(),
            body: None,
        }
    }

    /// 添加单个 header
    ///
    /// 如果已存在相同 key 的 header，将被覆盖。
    ///
    /// # Arguments
    ///
    /// * `key` - header 名称
    /// * `value` - header 值
    ///
    /// # Returns
    ///
    /// Self，支持链式调用
    pub fn header(mut self, key: &str, value: &str) -> Self {
        self.headers.insert(key.to_string(), value.to_string());
        self
    }

    /// 批量设置 headers
    ///
    /// 将传入的所有 headers 合并到现有 headers 中。
    /// 如果存在重复 key，新值会覆盖旧值。
    ///
    /// # Arguments
    ///
    /// * `headers` - 要添加的 headers 集合
    ///
    /// # Returns
    ///
    /// Self，支持链式调用
    pub fn headers(mut self, headers: HashMap<String, String>) -> Self {
        self.headers.extend(headers);
        self
    }

    /// 设置请求体
    ///
    /// # Arguments
    ///
    /// * `body` - 请求体的字节数据
    ///
    /// # Returns
    ///
    /// Self，支持链式调用
    pub fn body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }

    /// 构建最终的 HTTP 请求
    ///
    /// 自动添加以下标准 headers:
    /// - Host: 从 URL 中提取
    /// - User-Agent: aria2-rust/1.0
    /// - Accept: */*
    /// - Connection: close
    /// - Content-Length: 如果有 body
    ///
    /// # Returns
    ///
    /// 构建完成的 HttpRequest，或错误信息
    pub fn build(self) -> Result<HttpRequest> {
        let mut final_headers = self.headers;

        // 自动添加标准 headers (如果用户未手动设置)
        // Host header
        if !final_headers.contains_key("Host") {
            let host = self.url.host_str().unwrap_or("");
            if let Some(port) = self.url.port() {
                final_headers.insert("Host".to_string(), format!("{}:{}", host, port));
            } else {
                final_headers.insert("Host".to_string(), host.to_string());
            }
        }

        // User-Agent header
        if !final_headers.contains_key("User-Agent") {
            final_headers.insert(
                "User-Agent".to_string(),
                "aria2-rust/1.0".to_string(),
            );
        }

        // Accept header
        if !final_headers.contains_key("Accept") {
            final_headers.insert("Accept".to_string(), "*/*".to_string());
        }

        // Connection header
        if !final_headers.contains_key("Connection") {
            final_headers.insert("Connection".to_string(), "close".to_string());
        }

        // Content-Length header (如果有 body)
        if self.body.is_some() && !final_headers.contains_key("Content-Length") {
            let len = self.body.as_ref().unwrap().len();
            final_headers.insert("Content-Length".to_string(), len.to_string());
        }

        Ok(HttpRequest {
            method: self.method,
            url: self.url,
            headers: final_headers,
            body: self.body,
        })
    }
}

/// HTTP 响应结构体
///
/// 表示一个完整的 HTTP 响应，包含状态码、reason phrase、版本、headers 和可选的 body。
/// 支持多值 headers (如 Set-Cookie)。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// 状态码 (如 200, 404, 301)
    pub status_code: u16,
    /// 原因短语 (如 OK, Not Found, Moved Permanently)
    pub reason_phrase: String,
    /// HTTP 版本 (如 "HTTP/1.1")
    pub version: String,
    /// 响应 headers (支持多值)
    pub headers: HashMap<String, Vec<String>>,
    /// 可选的响应体
    pub body: Option<Vec<u8>>,
}

impl HttpResponse {
    /// 从原始字节解析 HTTP 响应
    ///
    /// 解析符合 HTTP/1.1 规范的响应数据，包括状态行、headers 和 body。
    /// 支持多值 headers (通过逗号分隔或多个同名 header)。
    ///
    /// # Arguments
    ///
    /// * `data` - 原始 HTTP 响应字节
    ///
    /// # Returns
    ///
    /// 解析后的 HttpResponse，或错误信息
    ///
    /// # Errors
    ///
    /// 返回错误如果响应格式无效或无法解析
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        let response_str =
            String::from_utf8(data.to_vec()).map_err(|e| Aria2Error::Parse(format!(
                "Invalid UTF-8 in HTTP response: {}",
                e
            )))?;

        // 分离 headers 和 body
        let (header_part, body_part) = match response_str.find("\r\n\r\n") {
            Some(pos) => (&response_str[..pos], &response_str[pos + 4..]),
            None => (response_str.as_str(), ""),
        };

        // 解析状态行
        let mut lines = header_part.split("\r\n");
        let status_line = lines
            .next()
            .ok_or_else(|| Aria2Error::Parse("Empty HTTP response".to_string()))?;

        // 解析 version, status_code, reason_phrase
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 {
            return Err(Aria2Error::Parse(
                "Invalid HTTP status line format".to_string(),
            ));
        }

        let version = parts[0].to_string();
        let status_code: u16 = parts[1]
            .parse()
            .map_err(|e| Aria2Error::Parse(format!("Invalid status code: {}", e)))?;
        let reason_phrase = if parts.len() > 2 {
            parts[2..].join(" ")
        } else {
            String::new()
        };

        // 解析 headers (支持多值)
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for line in lines {
            if line.is_empty() {
                continue;
            }
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim().to_string();
                let value = value.trim().to_string();
                headers
                    .entry(key)
                    .or_insert_with(Vec::new)
                    .push(value);
            }
        }

        // 处理 body
        let body = if body_part.is_empty() {
            None
        } else {
            Some(body_part.as_bytes().to_vec())
        };

        Ok(HttpResponse {
            status_code,
            reason_phrase,
            version,
            headers,
            body,
        })
    }

    /// 获取指定 header 的第一个值
    ///
    /// # Arguments
    ///
    /// * `name` - header 名称 (不区分大小写)
    ///
    /// # Returns
    ///
    /// 第一个 header 值的引用，如果不存在则返回 None
    pub fn header(&self, name: &str) -> Option<&String> {
        let name_lower = name.to_lowercase();
        for (key, values) in &self.headers {
            if key.to_lowercase() == name_lower {
                return values.first();
            }
        }
        None
    }

    /// 获取指定 header 的所有值
    ///
    /// 对于 Set-Cookie 等可能多次出现的 header 特别有用。
    ///
    /// # Arguments
    ///
    /// * `name` - header 名称 (不区分大小写)
    ///
    /// # Returns
    ///
    /// 包含所有匹配值的向量
    pub fn header_all(&self, name: &str) -> Vec<String> {
        let name_lower = name.to_lowercase();
        for (key, values) in &self.headers {
            if key.to_lowercase() == name_lower {
                return values.clone();
            }
        }
        Vec::new()
    }

    /// 获取 Content-Length header 的值
    ///
    /// # Returns
    ///
    /// 内容长度 (u64)，如果不存在或解析失败则返回 None
    pub fn content_length(&self) -> Option<u64> {
        self.header("Content-Length")
            .and_then(|v| v.parse::<u64>().ok())
    }

    /// 检查是否为重定向响应 (3xx)
    ///
    /// # Returns
    ///
    /// 如果状态码在 300-399 范围内返回 true
    pub fn is_redirect(&self) -> bool {
        (300..400).contains(&self.status_code)
    }

    /// 获取 Location header 并解析为 URL
    ///
    /// 对于重定向响应特别有用。如果是相对 URL，会基于当前请求 URL 进行解析。
    ///
    /// # Returns
    ///
    /// 解析后的绝对 URL，如果不存在或解析失败则返回 None
    pub fn location(&self) -> Option<Url> {
        self.header("Location").and_then(|loc| Url::parse(loc).ok())
    }
}

/// Cookie 结构体 (RFC 6265 合规)
///
/// 表示一个 HTTP cookie，包含名称、值、域、路径等属性。
#[derive(Debug, Clone)]
pub struct Cookie {
    /// Cookie 名称
    pub name: String,
    /// Cookie 值
    pub value: String,
    /// Cookie 所属域
    pub domain: String,
    /// Cookie 有效路径
    pub path: String,
    /// 过期时间 (None 表示会话 cookie)
    pub expiry: Option<SystemTime>,
    /// 是否仅 HTTPS 传输
    pub secure: bool,
    /// 是否禁止 JavaScript 访问
    pub http_only: bool,
}

impl Cookie {
    /// 创建新的 Cookie
    ///
    /// # Arguments
    ///
    /// * `name` - Cookie 名称
    /// * `value` - Cookie 值
    /// * `domain` - 所属域
    /// * `path` - 有效路径
    ///
    /// # Returns
    ///
    /// 新的 Cookie 实例
    pub fn new(name: &str, value: &str, domain: &str, path: &str) -> Self {
        Cookie {
            name: name.to_string(),
            value: value.to_string(),
            domain: domain.to_string().to_lowercase(),
            path: path.to_string(),
            expiry: None,
            secure: false,
            http_only: false,
        }
    }

    /// 检查 Cookie 是否已过期
    ///
    /// # Returns
    ///
    /// 如果 Cookie 已过期返回 true
    pub fn is_expired(&self) -> bool {
        if let Some(expiry) = self.expiry {
            SystemTime::now() > expiry
        } else {
            false // 会话 cookie 永不过期
        }
    }

    /// 检查域名是否匹配
    ///
    /// 根据 RFC 6265 规范检查域名是否匹配 cookie 的域属性。
    /// 支持精确匹配和子域匹配 (以 . 开头的域)。
    ///
    /// # Arguments
    ///
    /// * `host` - 要检查的主机名
    ///
    /// # Returns
    ///
    /// 如果域名匹配返回 true
    fn domain_matches(&self, host: &str) -> bool {
        let cookie_domain = self.domain.to_lowercase();
        let host = host.to_lowercase();

        // 精确匹配
        if cookie_domain == host {
            return true;
        }

        // 子域匹配 (cookie domain 以 . 开头)
        if cookie_domain.starts_with('.') {
            // .example.com 匹配 example.com 和 sub.example.com
            host == &cookie_domain[1..] || host.ends_with(&cookie_domain)
        } else {
            // 不以 . 开头的 domain，需要 host 以 .domain 结尾
            host.ends_with(&format!(".{}", cookie_domain))
        }
    }

    /// 检查路径是否匹配
    ///
    /// 根据 RFC 6265 规范检查路径是否匹配 cookie 的路径属性。
    ///
    /// # Arguments
    ///
    /// * `request_path` - 请求路径
    ///
    /// # Returns
    ///
    /// 如果路径匹配返回 true
    fn path_matches(&self, request_path: &str) -> bool {
        // "/" 匹配所有路径
        if self.path == "/" {
            return true;
        }

        // cookie 路径是请求路径的前缀
        if request_path.starts_with(&self.path) {
            // 确保边界对齐: /foo 匹配 /foobar 不合法，但 /foo/ 匹配 /foo/bar 合法
            if request_path.len() == self.path.len() {
                return true; // 精确匹配
            }
            // 检查 cookie 路径是否以 / 结尾，或者请求路径的下一个字符是 /
            if self.path.ends_with('/') || request_path.chars().nth(self.path.len()) == Some('/') {
                return true;
            }
        }
        false
    }

    /// 检查 Cookie 是否应该发送给指定的 URL
    ///
    /// 综合考虑域名、路径、安全标志等因素。
    ///
    /// # Arguments
    ///
    /// * `url` - 目标 URL
    ///
    /// # Returns
    ///
    /// 如果应该发送此 Cookie 返回 true
    pub fn should_send_to(&self, url: &Url) -> bool {
        // 安全标志检查
        if self.secure && url.scheme() != "https" {
            return false;
        }

        // 过期检查
        if self.is_expired() {
            return false;
        }

        // 域名匹配
        let host = url.host_str().unwrap_or("");
        if !self.domain_matches(host) {
            return false;
        }

        // 路径匹配
        let path = url.path();
        if !self.path_matches(path) {
            return false;
        }

        true
    }
}

/// Cookie Jar (RFC 6265 合规的 Cookie 存储和管理)
///
/// 用于存储和管理 HTTP cookies，支持从 Set-Cookie header 解析，
/// 以及根据请求 URL 获取合适的 cookies。
#[derive(Debug, Clone)]
pub struct CookieJar {
    /// 存储的所有 cookies
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// 创建新的空 Cookie Jar
    ///
    /// # Returns
    ///
    /// 新的 CookieJar 实例
    pub fn new() -> Self {
        CookieJar {
            cookies: Vec::new(),
        }
    }

    /// 从 Set-Cookie header 添加 Cookie
    ///
    /// 解析 Set-Cookie header 的值并存储到 jar 中。
    /// 自动从请求 URL 提取默认的 domain 和 path。
    ///
    /// # Arguments
    ///
    /// * `set_cookie_header` - Set-Cookie header 的完整值
    /// * `request_url` - 触发 Set-Cookie 的请求 URL
    pub fn set_cookie(&mut self, set_cookie_header: &str, request_url: &Url) {
        let header = set_cookie_header.trim();
        if header.is_empty() {
            return;
        }

        // 解析 name=value 部分
        let (name_value, attributes) = match header.find(';') {
            Some(pos) => (&header[..pos], &header[pos + 1..]),
            None => (header, ""),
        };

        // 解析 name=value
        let eq_pos = match name_value.find('=') {
            Some(pos) => pos,
            None => return, // 无效格式
        };

        let name = name_value[..eq_pos].trim();
        let value = name_value[eq_pos + 1..].trim();

        if name.is_empty() {
            return;
        }

        // 获取默认 domain 和 path
        let default_domain = request_url.host_str().unwrap_or("").to_string();
        let default_path = request_url.path().to_string();

        // 创建 cookie
        let mut cookie = Cookie::new(name, value, &default_domain, &default_path);

        // 解析属性
        for attr in attributes.split(';') {
            let attr = attr.trim();
            if attr.is_empty() {
                continue;
            }

            if let Some((attr_name, attr_value)) = attr.split_once('=') {
                match attr_name.trim().to_lowercase().as_str() {
                    "domain" => {
                        let domain = attr_value.trim();
                        // 确保 domain 以 . 开头
                        if domain.starts_with('.') {
                            cookie.domain = domain.to_lowercase();
                        } else {
                            cookie.domain = format!(".{}", domain).to_lowercase();
                        }
                    }
                    "path" => {
                        cookie.path = attr_value.trim().to_string();
                    }
                    "max-age" => {
                        if let Ok(seconds) = attr_value.trim().parse::<u64>() {
                            let expiry = SystemTime::now()
                                + std::time::Duration::from_secs(seconds);
                            cookie.expiry = Some(expiry);
                        }
                    }
                    "expires" => {
                        // 简单实现: 尝试解析常见日期格式
                        // 完整实现需要更复杂的日期解析
                        if let Ok(timestamp) = attr_value.trim().parse::<i64>() {
                            let expiry = UNIX_EPOCH + std::time::Duration::from_secs(timestamp as u64);
                            cookie.expiry = Some(expiry);
                        }
                    }
                    _ => {} // 忽略未知属性
                }
            } else {
                // 无值的布尔属性
                match attr.to_lowercase().as_str() {
                    "secure" => cookie.secure = true,
                    "httponly" => cookie.http_only = true,
                    _ => {} // 忽略未知属性
                }
            }
        }

        // 移除同名的旧 cookie (同一 domain + path + name)
        self.cookies.retain(|c| {
            !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        });

        // 添加新 cookie
        self.cookies.push(cookie);
    }

    /// 获取适用于指定 URL 的所有 Cookies
    ///
    /// 根据域名、路径和安全标志筛选合适的 cookies，
    /// 并返回格式化为 `Cookie:` header 的字符串。
    ///
    /// # Arguments
    ///
    /// * `url` - 目标 URL
    ///
    /// # Returns
    ///
    /// 格式化为 `name1=value1; name2=value2` 的字符串
    pub fn get_cookies_for_url(&self, url: &Url) -> String {
        let matching_cookies: Vec<&Cookie> = self
            .cookies
            .iter()
            .filter(|c| c.should_send_to(url))
            .collect();

        if matching_cookies.is_empty() {
            return String::new();
        }

        matching_cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// 清除所有过期的 Cookies
    ///
    /// 移除所有已过期的持久性 cookies。会话 cookies (无过期时间) 不会被移除。
    pub fn cleanup_expired(&mut self) {
        self.cookies.retain(|c| !c.is_expired());
    }

    /// 获取存储的 Cookies 数量
    ///
    /// # Returns
    ///
    /// 当前存储的 cookie 数量
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// 检查 Cookie Jar 是否为空
    ///
    /// # Returns
    ///
    /// 如果没有存储任何 cookie 返回 true
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}

/// 认证头构建工具
///
/// 提供 Basic Auth 等 HTTP 认证方案的 header 生成功能。
pub struct AuthHeaderBuilder;

impl AuthHeaderBuilder {
    /// 生成 Basic Auth 认证头
    ///
    /// 将用户名和密码进行 Base64 编码生成 `Authorization: Basic <credentials>` header。
    ///
    /// # Arguments
    ///
    /// * `username` - 用户名
    /// * `password` - 密码
    ///
    /// # Returns
    ///
    /// 完整的 Authorization header 值 (如 `Basic dXNlcjpwYXNz`)
    ///
    /// # Examples
    ///
    /// ```
    /// use aria2_core::http::request_response::AuthHeaderBuilder;
    ///
    /// let auth_header = AuthHeaderBuilder::basic_auth("user", "pass");
    /// assert_eq!(auth_header, "Basic dXNlcjpwYXNz");
    /// ```
    pub fn basic_auth(username: &str, password: &str) -> String {
        let credentials = format!("{}:{}", username, password);
        let encoded = general_purpose::STANDARD.encode(credentials.as_bytes());
        format!("Basic {}", encoded)
    }

    /// 生成 Bearer Token 认证头
    ///
    /// # Arguments
    ///
    /// * `token` - Bearer token
    ///
    /// # Returns
    ///
    /// 完整的 Authorization header 值
    pub fn bearer_token(token: &str) -> String {
        format!("Bearer {}", token)
    }
}

// Base64 编码已通过 use base64::{engine::general_purpose, Engine} 导入

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_builder_fluent_api() {
        let url = Url::parse("http://example.com/api/test").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Post, url.clone())
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(b"{\"key\":\"value\"}".to_vec())
            .build()
            .unwrap();

        assert_eq!(request.method, HttpMethod::Post);
        assert_eq!(request.url, url);
        assert_eq!(request.headers.get("Content-Type").unwrap(), "application/json");
        assert_eq!(request.headers.get("Accept").unwrap(), "application/json");
        assert!(request.body.is_some());
        assert_eq!(request.body.unwrap(), b"{\"key\":\"value\"}");
    }

    #[test]
    fn test_request_auto_headers_generation() {
        let url = Url::parse("http://example.com:8080/path").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .build()
            .unwrap();

        // 检查自动生成的 Host header (包含端口)
        assert_eq!(request.headers.get("Host").unwrap(), "example.com:8080");

        // 检查自动生成的 User-Agent
        assert_eq!(request.headers.get("User-Agent").unwrap(), "aria2-rust/1.0");

        // 检查自动生成的 Accept
        assert_eq!(request.headers.get("Accept").unwrap(), "*/*");

        // 检查自动生成的 Connection
        assert_eq!(request.headers.get("Connection").unwrap(), "close");
    }

    #[test]
    fn test_request_auto_content_length() {
        let url = Url::parse("http://example.com/api").unwrap();
        let body = b"test body data";
        let request = HttpRequestBuilder::new(HttpMethod::Post, url)
            .body(body.to_vec())
            .build()
            .unwrap();

        assert_eq!(
            request.headers.get("Content-Length").unwrap(),
            &body.len().to_string()
        );
    }

    #[test]
    fn test_request_custom_host_not_overridden() {
        let url = Url::parse("http://example.com/api").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .header("Host", "custom-host.com")
            .build()
            .unwrap();

        // 用户自定义的 Host 应该保留
        assert_eq!(request.headers.get("Host").unwrap(), "custom-host.com");
    }

    #[test]
    fn test_request_to_bytes() {
        let url = Url::parse("http://example.com/path?q=1").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .header("Custom-Header", "test-value")
            .build()
            .unwrap();

        let bytes = request.to_bytes();
        let request_str = String::from_utf8(bytes).unwrap();

        // 验证请求行
        assert!(request_str.starts_with("GET /path?q=1 HTTP/1.1\r\n"));

        // 验证自定义 header
        assert!(request_str.contains("Custom-Header: test-value"));

        // 验证标准 headers
        assert!(request_str.contains("Host: example.com"));
        assert!(request_str.contains("User-Agent: aria2-rust/1.0"));
    }

    #[test]
    fn test_request_to_bytes_with_body() {
        let url = Url::parse("http://example.com/api").unwrap();
        let request = HttpRequestBuilder::new(HttpMethod::Post, url)
            .header("Content-Type", "text/plain")
            .body(b"Hello, World!".to_vec())
            .build()
            .unwrap();

        let bytes = request.to_bytes();
        let request_str = String::from_utf8_lossy(&bytes);

        assert!(request_str.contains("POST /api HTTP/1.1"));
        assert!(request_str.contains("Content-Length: 13"));
        assert!(request_str.ends_with("Hello, World!"));
    }

    #[test]
    fn test_response_status_parsing() {
        // 测试 200 OK
        let response_200 = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<body>";
        let resp = HttpResponse::from_bytes(response_200.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.reason_phrase, "OK");
        assert_eq!(resp.version, "HTTP/1.1");

        // 测试 404 Not Found
        let response_404 = "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\n\r\nNot Found";
        let resp = HttpResponse::from_bytes(response_404.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 404);
        assert_eq!(resp.reason_phrase, "Not Found");

        // 测试 301 Moved Permanently
        let response_301 = "HTTP/1.1 301 Moved Permanently\r\nLocation: /new-url\r\n\r\n";
        let resp = HttpResponse::from_bytes(response_301.as_bytes()).unwrap();
        assert_eq!(resp.status_code, 301);
        assert_eq!(resp.reason_phrase, "Moved Permanently");
    }

    #[test]
    fn test_response_multi_value_headers() {
        let response = "HTTP/1.1 200 OK\r\n\
                       Set-Cookie: session=abc123; Path=/\r\n\
                       Set-Cookie: user=john; Domain=example.com\r\n\
                       Content-Type: text/html\r\n\r\n<body>";

        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        // 测试获取所有 Set-Cookie 值
        let all_cookies = resp.header_all("Set-Cookie");
        assert_eq!(all_cookies.len(), 2);
        assert!(all_cookies.contains(&"session=abc123; Path=/".to_string()));
        assert!(all_cookies.contains(&"user=john; Domain=example.com".to_string()));

        // 测试获取第一个值
        let first_cookie = resp.header("Set-Cookie").unwrap();
        assert_eq!(first_cookie, "session=abc123; Path=/");
    }

    #[test]
    fn test_response_content_length() {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 1024\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert_eq!(resp.content_length(), Some(1024));

        // 无 Content-Length
        let response_no_cl = "HTTP/1.1 200 OK\r\n\r\n";
        let resp_no_cl = HttpResponse::from_bytes(response_no_cl.as_bytes()).unwrap();
        assert_eq!(resp_no_cl.content_length(), None);
    }

    #[test]
    fn test_response_is_redirect() {
        // 重定向状态码
        let redirect_resp = HttpResponse::from_bytes(
            "HTTP/1.1 301 Moved Permanently\r\nLocation: /new\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert!(redirect_resp.is_redirect());

        let redirect_302 = HttpResponse::from_bytes("HTTP/1.1 302 Found\r\n\r\n".as_bytes()).unwrap();
        assert!(redirect_302.is_redirect());

        // 非重定向状态码
        let ok_resp = HttpResponse::from_bytes("HTTP/1.1 200 OK\r\n\r\n".as_bytes()).unwrap();
        assert!(!ok_resp.is_redirect());

        let error_resp = HttpResponse::from_bytes("HTTP/1.1 500 Internal Server Error\r\n\r\n".as_bytes()).unwrap();
        assert!(!error_resp.is_redirect());
    }

    #[test]
    fn test_response_location() {
        let response = "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/new-page\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        let location = resp.location().unwrap();
        assert_eq!(location.as_str(), "https://example.com/new-page");
    }

    #[test]
    fn test_response_body_parsing() {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"success\"}";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert!(resp.body.is_some());
        assert_eq!(resp.body.unwrap(), b"{\"status\":\"success\"}");

        // 无 body
        let response_no_body = "HTTP/1.1 204 No Content\r\n\r\n";
        let resp_no_body = HttpResponse::from_bytes(response_no_body.as_bytes()).unwrap();
        assert!(resp_no_body.body.is_none());
    }

    #[test]
    fn test_cookie_jar_basic_operations() {
        let mut jar = CookieJar::new();
        assert!(jar.is_empty());
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn test_cookie_jar_set_and_get() {
        let mut jar = CookieJar::new();
        let url = Url::parse("http://example.com/page").unwrap();

        jar.set_cookie("session=abc123; Path=/", &url);
        jar.set_cookie("user=john; Domain=.example.com", &url);

        assert_eq!(jar.len(), 2);

        let cookies = jar.get_cookies_for_url(&url);
        assert!(cookies.contains("session=abc123"));
        assert!(cookies.contains("user=john"));
    }

    #[test]
    fn test_cookie_jar_domain_match() {
        let mut jar = CookieJar::new();
        let url = Url::parse("http://example.com/page").unwrap();

        // 设置一个针对 example.com 及其子域的 cookie (显式指定 Path=/)
        jar.set_cookie("token=xyz; Domain=.example.com; Path=/", &url);

        // 主域应该能获取到
        let main_url = Url::parse("http://example.com/resource").unwrap();
        let cookies = jar.get_cookies_for_url(&main_url);
        assert!(cookies.contains("token=xyz"));

        // 子域也应该能获取到
        let sub_url = Url::parse("http://sub.example.com/resource").unwrap();
        let cookies_sub = jar.get_cookies_for_url(&sub_url);
        assert!(cookies_sub.contains("token=xyz"));

        // 不同域不应该获取到
        let other_url = Url::parse("http://other.com/resource").unwrap();
        let cookies_other = jar.get_cookies_for_url(&other_url);
        assert!(!cookies_other.contains("token=xyz"));
    }

    #[test]
    fn test_cookie_jar_path_match() {
        let mut jar = CookieJar::new();
        let url = Url::parse("http://example.com/page").unwrap();

        // 设置特定路径的 cookie
        jar.set_cookie("pref=en; Path=/api", &url);

        // 匹配 /api 及其子路径
        let api_url = Url::parse("http://example.com/api/users").unwrap();
        let cookies_api = jar.get_cookies_for_url(&api_url);
        assert!(cookies_api.contains("pref=en"));

        // 不匹配其他路径
        let home_url = Url::parse("http://example.com/home").unwrap();
        let cookies_home = jar.get_cookies_for_url(&home_url);
        assert!(!cookies_home.contains("pref=en"));
    }

    #[test]
    fn test_cookie_secure_flag() {
        let mut jar = CookieJar::new();
        let url = Url::parse("https://secure.example.com/api").unwrap();

        // 设置 secure cookie
        jar.set_cookie("secret=token; Secure; Domain=.example.com", &url);

        // HTTPS 请求可以获取
        let https_url = Url::parse("https://secure.example.com/api").unwrap();
        let cookies_https = jar.get_cookies_for_url(&https_url);
        assert!(cookies_https.contains("secret=token"));

        // HTTP 请求不能获取
        let http_url = Url::parse("http://secure.example.com/api").unwrap();
        let cookies_http = jar.get_cookies_for_url(&http_url);
        assert!(!cookies_http.contains("secret=token"));
    }

    #[test]
    fn test_cookie_expiry_cleanup() {
        let mut jar = CookieJar::new();
        let url = Url::parse("http://example.com/page").unwrap();

        // 设置一个立即过期的 cookie (Max-Age: 0)
        jar.set_cookie("expired=value; Max-Age=0", &url);

        // 设置一个不会过期的 session cookie
        jar.set_cookie("session=active", &url);

        assert_eq!(jar.len(), 2);

        // 清理过期 cookie
        jar.cleanup_expired();

        // 应该只剩 session cookie
        assert_eq!(jar.len(), 1);
        let cookies = jar.get_cookies_for_url(&url);
        assert!(cookies.contains("session=active"));
        assert!(!cookies.contains("expired=value"));
    }

    #[test]
    fn test_cookie_replace_same_name_domain_path() {
        let mut jar = CookieJar::new();
        let url = Url::parse("http://example.com/page").unwrap();

        // 第一次设置
        jar.set_cookie("session=first", &url);
        assert_eq!(jar.len(), 1);

        // 相同 name+domain+path 的 cookie 应该替换旧的
        jar.set_cookie("session=second", &url);
        assert_eq!(jar.len(), 1);

        let cookies = jar.get_cookies_for_url(&url);
        assert!(cookies.contains("session=second"));
        assert!(!cookies.contains("session=first"));
    }

    #[test]
    fn test_basic_auth_header_generation() {
        // 测试基本的 Base64 编码
        let auth = AuthHeaderBuilder::basic_auth("user", "pass");
        assert_eq!(auth, "Basic dXNlcjpwYXNz");

        // 测试特殊字符
        let auth_special = AuthHeaderBuilder::basic_auth("admin@email.com", "p@ssw0rd!");
        // 验证格式正确
        assert!(auth_special.starts_with("Basic "));
        // 验证可以解码回原始凭证
        let encoded = &auth_special["Basic ".len()..];
        let decoded = String::from_utf8(
            general_purpose::STANDARD
                .decode(encoded)
                .unwrap_or_default(),
        )
        .unwrap_or_default();
        assert_eq!(decoded, "admin@email.com:p@ssw0rd!");

        // 测试空密码
        let auth_empty_pass = AuthHeaderBuilder::basic_auth("user", "");
        assert!(auth_empty_pass.starts_with("Basic "));
    }

    #[test]
    fn test_bearer_token_generation() {
        let token = AuthHeaderBuilder::bearer_token("my-access-token-12345");
        assert_eq!(token, "Bearer my-access-token-12345");
    }

    #[test]
    fn test_http_method_display() {
        assert_eq!(HttpMethod::Get.to_string(), "GET");
        assert_eq!(HttpMethod::Post.to_string(), "POST");
        assert_eq!(HttpMethod::Head.to_string(), "HEAD");
        assert_eq!(HttpMethod::Put.to_string(), "PUT");
        assert_eq!(HttpMethod::Delete.to_string(), "DELETE");
    }

    #[test]
    fn test_request_builder_batch_headers() {
        let url = Url::parse("http://example.com/api").unwrap();
        let mut custom_headers = HashMap::new();
        custom_headers.insert("X-Custom-1".to_string(), "value1".to_string());
        custom_headers.insert("X-Custom-2".to_string(), "value2".to_string());

        let request = HttpRequestBuilder::new(HttpMethod::Get, url)
            .headers(custom_headers.clone())
            .build()
            .unwrap();

        assert_eq!(request.headers.get("X-Custom-1").unwrap(), "value1");
        assert_eq!(request.headers.get("X-Custom-2").unwrap(), "value2");
    }

    #[test]
    fn test_response_case_insensitive_headers() {
        let response = "HTTP/1.1 200 OK\r\n\
                       Content-Type: text/html\r\n\
                       content-length: 100\r\n\r\n";

        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        // 大小写不敏感查找
        assert!(resp.header("content-type").is_some());
        assert!(resp.header("CONTENT-TYPE").is_some());
        assert!(resp.header("Content-Length").is_some());
    }

    #[test]
    fn test_cookie_httponly_flag() {
        let mut jar = CookieJar::new();
        let url = Url::parse("http://example.com/page").unwrap();

        // HttpOnly 只是标记，不影响发送逻辑 (浏览器层面限制 JavaScript 访问)
        jar.set_cookie("session=abc; HttpOnly", &url);

        let cookies = jar.get_cookies_for_url(&url);
        assert!(cookies.contains("session=abc"));
    }

    #[test]
    fn test_full_request_response_cycle() {
        // 模拟完整的请求-响应周期
        let url = Url::parse("http://example.com/api/data").unwrap();

        // 构建请求
        let request = HttpRequestBuilder::new(HttpMethod::Get, url.clone())
            .header("Accept", "application/json")
            .build()
            .unwrap();

        // 序列化请求
        let request_bytes = request.to_bytes();
        let request_str = String::from_utf8_lossy(&request_bytes);
        assert!(request_str.contains("GET /api/data HTTP/1.1"));
        assert!(request_str.contains("Accept: application/json"));

        // 模拟服务器响应
        let response_data = "HTTP/1.1 200 OK\r\n\
                           Content-Type: application/json\r\n\
                           Set-Cookie: session=xyz; Path=/\r\n\
                           Content-Length: 17\r\n\r\n\
                           {\"data\":\"test\"}";

        // 解析响应
        let response = HttpResponse::from_bytes(response_data.as_bytes()).unwrap();
        assert_eq!(response.status_code, 200);
        assert_eq!(response.header("Content-Type").unwrap(), "application/json");
        assert_eq!(response.header_all("Set-Cookie").len(), 1);
        assert!(response.body.is_some());

        // 处理 Set-Cookie
        let mut jar = CookieJar::new();
        jar.set_cookie(response.header("Set-Cookie").unwrap(), &url);

        // 后续请求应该携带 cookie
        let next_request = HttpRequestBuilder::new(HttpMethod::Get, url.clone())
            .header("Cookie", &jar.get_cookies_for_url(&url))
            .build()
            .unwrap();

        assert_eq!(
            next_request.headers.get("Cookie").unwrap(),
            "session=xyz"
        );
    }
}
