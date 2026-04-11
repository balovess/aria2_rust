use std::time::Duration;

use reqwest::{Certificate, Client, ClientBuilder, redirect};
use tracing::{debug, info};

use crate::http::request::HttpRequest;
use crate::http::response::HttpResponse;

#[derive(Debug, Clone)]
pub struct HttpClientOptions {
    pub connect_timeout: Duration,
    pub timeout: Duration,
    pub max_redirects: usize,
    pub user_agent: String,
    pub accept_gzip: bool,
    pub verify_tls: bool,
    pub ca_cert_path: Option<String>,
}

impl Default for HttpClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            timeout: Duration::from_secs(300),
            max_redirects: 5,
            user_agent: "aria2/1.37.0-Rust".to_string(),
            accept_gzip: true,
            verify_tls: true,
            ca_cert_path: None,
        }
    }
}

pub struct HttpClient {
    inner: Client,
    options: HttpClientOptions,
}

impl HttpClient {
    pub fn new(options: HttpClientOptions) -> Result<Self, String> {
        let mut builder = ClientBuilder::new()
            .connect_timeout(options.connect_timeout)
            .timeout(options.timeout)
            .user_agent(&options.user_agent)
            .redirect(redirect::Policy::limited(options.max_redirects));

        if options.accept_gzip {
            builder = builder.gzip(true);
        }

        if !options.verify_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if let Some(ref ca_path) = options.ca_cert_path {
            match std::fs::read(ca_path) {
                Ok(cert_bytes) => {
                    let cert = Certificate::from_pem(&cert_bytes)
                        .map_err(|e| format!("加载CA证书失败: {}", e))?;
                    builder = builder.add_root_certificate(cert);
                }
                Err(e) => return Err(format!("读取CA证书文件失败: {}", e)),
            }
        }

        let inner = builder
            .build()
            .map_err(|e| format!("创建HTTP客户端失败: {}", e))?;

        info!(
            "HttpClient初始化完成 (超时={:?}, 重定向上限={}, TLS验证={})",
            options.timeout, options.max_redirects, options.verify_tls
        );

        Ok(Self { inner, options })
    }

    pub fn default_client() -> Result<Self, String> {
        Self::new(HttpClientOptions::default())
    }

    pub async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        debug!("发送HTTP请求: {} {}", request.method, request.url);

        let mut reqwest_request = match request.method.to_uppercase().as_str() {
            "GET" => self.inner.get(&request.url),
            "POST" => self.inner.post(&request.url),
            "HEAD" => self.inner.head(&request.url),
            "PUT" => self.inner.put(&request.url),
            _ => return Err(format!("不支持的HTTP方法: {}", request.method)),
        };

        if let Some(ref headers) = request.headers {
            for (key, value) in headers.iter() {
                reqwest_request = reqwest_request.header(
                    key.as_str()
                        .parse::<reqwest::header::HeaderName>()
                        .map_err(|e| format!("无效的请求头名称: {}", e))?,
                    value.as_str(),
                );
            }
        }

        if let Some(body) = request.body {
            reqwest_request = reqwest_request.body(body);
        }

        let response = reqwest_request
            .send()
            .await
            .map_err(|e| format!("HTTP请求失败: {}", e))?;

        let status = response.status();
        let status_code = status.as_u16();
        debug!("收到HTTP响应: 状态码={}", status_code);

        let headers_map: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| format!("读取响应体失败: {}", e))?
            .to_vec();

        Ok(HttpResponse {
            status_code,
            status_text: status.canonical_reason().unwrap_or("Unknown").to_string(),
            headers: headers_map,
            body: body_bytes,
        })
    }

    pub fn get<U: Into<String>>(&self, url: U) -> HttpRequestBuilder<'_> {
        HttpRequestBuilder::new(self, "GET", url.into())
    }

    pub fn post<U: Into<String>>(&self, url: U) -> HttpRequestBuilder<'_> {
        HttpRequestBuilder::new(self, "POST", url.into())
    }

    pub fn head<U: Into<String>>(&self, url: U) -> HttpRequestBuilder<'_> {
        HttpRequestBuilder::new(self, "HEAD", url.into())
    }

    pub fn options_ref(&self) -> &HttpClientOptions {
        &self.options
    }
}

pub struct HttpRequestBuilder<'a> {
    client: &'a HttpClient,
    method: String,
    url: String,
    headers: Option<reqwest::header::HeaderMap>,
    body: Option<Vec<u8>>,
}

impl<'a> HttpRequestBuilder<'a> {
    fn new(client: &'a HttpClient, method: &str, url: String) -> Self {
        Self {
            client,
            method: method.to_string(),
            url,
            headers: None,
            body: None,
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        let mut headers = self.headers.take().unwrap_or_default();
        headers.insert(
            name.parse::<reqwest::header::HeaderName>()
                .expect("无效的请求头名称"),
            value
                .parse::<reqwest::header::HeaderValue>()
                .expect("无效的请求头值"),
        );
        self.headers = Some(headers);
        self
    }

    pub fn header_raw<K, V>(mut self, key: K, value: V) -> Self
    where
        K: TryInto<reqwest::header::HeaderName>,
        V: TryInto<reqwest::header::HeaderValue>,
    {
        let mut headers = self.headers.take().unwrap_or_default();
        if let (Ok(k), Ok(v)) = (key.try_into(), value.try_into()) {
            headers.insert(k, v);
        }
        self.headers = Some(headers);
        self
    }

    pub fn range(self, start: u64, end: Option<u64>) -> Self {
        let range_value = match end {
            Some(e) => format!("bytes={}-{}", start, e),
            None => format!("bytes={}-", start),
        };
        self.header("Range", &range_value)
    }

    pub fn body<B: Into<Vec<u8>>>(mut self, body: B) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn user_agent(self, ua: &str) -> Self {
        self.header("User-Agent", ua)
    }

    pub fn referer(self, referer: &str) -> Self {
        self.header("Referer", referer)
    }

    pub async fn send(self) -> Result<HttpResponse, String> {
        let headers_map = self.headers.map(|h| {
            h.iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>()
        });

        let request = HttpRequest {
            method: self.method.clone(),
            url: self.url.clone(),
            headers: headers_map,
            body: self.body,
        };

        self.client.execute(request).await
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RedirectPolicy {
    Follow,
    Limit(usize),
    None,
}

impl Default for RedirectPolicy {
    fn default() -> Self {
        Self::Limit(5)
    }
}

pub struct RedirectHandler;

impl RedirectHandler {
    pub fn should_follow_redirect(
        status_code: u16,
        _method: &str,
        current_redirects: usize,
        max_redirects: usize,
    ) -> Option<RedirectAction> {
        if current_redirects >= max_redirects {
            debug!("达到最大重定向次数上限: {}", max_redirects);
            return None;
        }

        match status_code {
            301 => Some(RedirectAction::FollowKeepMethod),
            302 | 303 => Some(RedirectAction::FollowChangeToGet),
            307 | 308 => Some(RedirectAction::FollowKeepMethod),
            _ => None,
        }
    }

    pub fn resolve_redirect_url(current_url: &str, location: &str) -> Result<String, String> {
        let location = location.trim();
        if location.is_empty() {
            return Err("Location头为空".to_string());
        }

        if location.starts_with("http://") || location.starts_with("https://") {
            return Ok(location.to_string());
        }

        if location.starts_with("/") {
            if let Some(base_end) = current_url[8..].find('/') {
                Ok(format!("{}{}", &current_url[..8 + base_end], location))
            } else {
                Ok(format!("{}{}", current_url, location))
            }
        } else {
            let last_slash = current_url.rfind('/').unwrap_or(current_url.len());
            if last_slash < 8 {
                Ok(format!("{}/{}", current_url, location))
            } else {
                Ok(format!("{}{}", &current_url[..last_slash + 1], location))
            }
        }
    }

    pub fn sanitize_redirect_url(url: &str) -> Result<String, String> {
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("不安全的重定向URL协议: {}", url));
        }
        Ok(url.to_string())
    }
}

pub enum RedirectAction {
    FollowKeepMethod,
    FollowChangeToGet,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_should_follow_301() {
        let action = RedirectHandler::should_follow_redirect(301, "GET", 0, 5);
        assert!(action.is_some());

        let no_action = RedirectHandler::should_follow_redirect(301, "GET", 5, 5);
        assert!(no_action.is_none());
    }

    #[test]
    fn test_should_not_follow_200() {
        let action = RedirectHandler::should_follow_redirect(200, "GET", 0, 5);
        assert!(action.is_none());
    }

    #[test]
    fn test_resolve_absolute_redirect() {
        let url = RedirectHandler::resolve_redirect_url(
            "http://example.com/page",
            "http://other.example.com/new",
        )
        .unwrap();
        assert_eq!(url, "http://other.example.com/new");
    }

    #[test]
    fn test_resolve_relative_redirect() {
        let url = RedirectHandler::resolve_redirect_url("http://example.com/old/path", "/new/path")
            .unwrap();
        assert_eq!(url, "http://example.com/new/path");
    }

    #[test]
    fn test_resolve_relative_path_redirect() {
        let url = RedirectHandler::resolve_redirect_url(
            "http://example.com/old/page.html",
            "new-page.html",
        )
        .unwrap();
        assert_eq!(url, "http://example.com/old/new-page.html");
    }

    #[test]
    fn test_302_changes_to_get() {
        let action = RedirectHandler::should_follow_redirect(302, "POST", 0, 5);
        assert!(action.is_some());
        if let Some(action) = action {
            assert!(matches!(action, RedirectAction::FollowChangeToGet));
        }
    }

    #[test]
    fn test_307_keeps_method() {
        let action = RedirectHandler::should_follow_redirect(307, "POST", 0, 5);
        assert!(action.is_some());
        if let Some(action) = action {
            assert!(matches!(action, RedirectAction::FollowKeepMethod));
        }
    }
}
