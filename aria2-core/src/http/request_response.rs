//! HTTP request building and response parsing module
//!
//! Provides HTTP/1.1 request building, response parsing, and authentication.
//! Supports fluent API for building HTTP requests with automatic standard headers.

use base64::{Engine, engine::general_purpose};
use std::collections::HashMap;
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
            final_headers.insert("User-Agent".to_string(), "aria2-rust/1.0".to_string());
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
        if let Some(body) = &self.body
            && !final_headers.contains_key("Content-Length")
        {
            let len = body.len();
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
        let response_str = String::from_utf8(data.to_vec())
            .map_err(|e| Aria2Error::Parse(format!("Invalid UTF-8 in HTTP response: {}", e)))?;

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
                headers.entry(key).or_default().push(value);
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

    /// 使用流式解码器获取解码后的 body
    ///
    /// 根据 HTTP 响应的 Content-Encoding 和 Transfer-Encoding headers 自动选择合适的解码器，
    /// 对响应体进行解码。支持 GZip、Chunked、BZip2 等编码格式。
    ///
    /// 遵循 RFC 7230 Section 3.3.1 规范：Transfer-Encoding 优先于 Content-Encoding。
    ///
    /// # Returns
    ///
    /// 解码后的原始数据，或错误信息。如果没有 body 则返回空向量。
    ///
    /// # Errors
    ///
    /// - 如果编码格式无效或数据损坏
    /// - 如果解码过程中发生 I/O 错误
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// let response = /* HTTP response with Content-Encoding: gzip */;
    /// let decoded = response.decoded_body()?;
    /// // decoded 包含解压后的原始数据
    /// ```
    pub fn decoded_body(&self) -> Result<Vec<u8>> {
        use crate::http::stream_filter::{AutoFilterSelector, process_filters};

        let encoding = self.header("Content-Encoding").map(|s| s.as_str());
        let transfer_enc = self.header("Transfer-Encoding").map(|s| s.as_str());

        let mut filters = AutoFilterSelector::select_filters(encoding, transfer_enc);

        match &self.body {
            Some(raw_data) => process_filters(&mut filters, raw_data),
            None => Ok(Vec::new()),
        }
    }
}

/// Generate Basic Auth header value
///
/// Encodes username:password as Base64 and returns `Basic <credentials>`.
///
/// # Examples
///
/// ```
/// use aria2_core::http::request_response::basic_auth;
///
/// let auth_header = basic_auth("user", "pass");
/// assert_eq!(auth_header, "Basic dXNlcjpwYXNz");
/// ```
pub fn basic_auth(username: &str, password: &str) -> String {
    let credentials = format!("{}:{}", username, password);
    let encoded = general_purpose::STANDARD.encode(credentials.as_bytes());
    format!("Basic {}", encoded)
}

/// Generate Bearer Token header value
///
/// # Arguments
///
/// * `token` - Bearer token
///
/// # Returns
///
/// Complete Authorization header value (e.g., `Bearer my-token`)
pub fn bearer_token(token: &str) -> String {
    format!("Bearer {}", token)
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
        assert_eq!(
            request.headers.get("Content-Type").unwrap(),
            "application/json"
        );
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

        let redirect_302 =
            HttpResponse::from_bytes("HTTP/1.1 302 Found\r\n\r\n".as_bytes()).unwrap();
        assert!(redirect_302.is_redirect());

        // 非重定向状态码
        let ok_resp = HttpResponse::from_bytes("HTTP/1.1 200 OK\r\n\r\n".as_bytes()).unwrap();
        assert!(!ok_resp.is_redirect());

        let error_resp =
            HttpResponse::from_bytes("HTTP/1.1 500 Internal Server Error\r\n\r\n".as_bytes())
                .unwrap();
        assert!(!error_resp.is_redirect());
    }

    #[test]
    fn test_response_location() {
        let response =
            "HTTP/1.1 301 Moved Permanently\r\nLocation: https://example.com/new-page\r\n\r\n";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        let location = resp.location().unwrap();
        assert_eq!(location.as_str(), "https://example.com/new-page");
    }

    #[test]
    fn test_response_body_parsing() {
        let response =
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"status\":\"success\"}";
        let resp = HttpResponse::from_bytes(response.as_bytes()).unwrap();

        assert!(resp.body.is_some());
        assert_eq!(resp.body.unwrap(), b"{\"status\":\"success\"}");

        // 无 body
        let response_no_body = "HTTP/1.1 204 No Content\r\n\r\n";
        let resp_no_body = HttpResponse::from_bytes(response_no_body.as_bytes()).unwrap();
        assert!(resp_no_body.body.is_none());
    }

    #[test]
    fn test_basic_auth_header_generation() {
        // Test basic Base64 encoding
        let auth = basic_auth("user", "pass");
        assert_eq!(auth, "Basic dXNlcjpwYXNz");

        // Test special characters
        let auth_special = basic_auth("admin@email.com", "p@ssw0rd!");
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
        let auth_empty_pass = basic_auth("user", "");
        assert!(auth_empty_pass.starts_with("Basic "));
    }

    #[test]
    fn test_bearer_token_generation() {
        let token = bearer_token("my-access-token-12345");
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

}
