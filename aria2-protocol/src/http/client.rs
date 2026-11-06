use std::sync::Once;
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{Certificate, Client, ClientBuilder, redirect};
use tracing::{debug, info};

use crate::http::request::HttpRequest;
use crate::http::response::{HttpResponse, is_redirect_status};

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

/// A response body stream returned by [`HttpClient::execute_stream`].
///
/// Response metadata is available immediately after headers arrive. Body
/// bytes are read on demand, so dropping this value cancels the in-flight
/// response instead of buffering the remaining payload.
pub struct HttpResponseStream {
    status_code: u16,
    status_text: String,
    headers: Vec<(String, String)>,
    body: HttpBodyStream,
}

/// The body stream type exposed by [`HttpResponseStream::into_stream`].
pub type HttpBodyStream = BoxStream<'static, Result<Bytes, String>>;

impl HttpResponseStream {
    /// Return the HTTP status code received from the server.
    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    /// Return the canonical status text received from the server.
    pub fn status_text(&self) -> &str {
        &self.status_text
    }

    /// Return the response headers captured before body streaming began.
    pub fn headers(&self) -> &[(String, String)] {
        &self.headers
    }

    /// Look up a response header without regard to ASCII case.
    pub fn header(&self, name: &str) -> Option<&String> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }

    /// Read the next body chunk.
    ///
    /// `None` means end-of-stream. Transport failures are returned as
    /// `Err(String)` and do not get silently converted into an empty body.
    pub async fn next_chunk(&mut self) -> Option<Result<Bytes, String>> {
        self.body.next().await
    }

    /// Take ownership of the underlying body stream.
    pub fn into_stream(self) -> HttpBodyStream {
        self.body
    }

    /// Collect the streamed body into the existing buffered response type.
    pub async fn collect(mut self) -> Result<HttpResponse, String> {
        let mut body = Vec::new();
        while let Some(chunk) = self.next_chunk().await {
            body.extend_from_slice(&chunk?);
        }

        Ok(HttpResponse {
            status_code: self.status_code,
            status_text: self.status_text,
            headers: self.headers,
            body,
        })
    }
}

impl Default for HttpClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            timeout: Duration::from_secs(300),
            max_redirects: 5,
            user_agent: crate::identity::DEFAULT_USER_AGENT.to_string(),
            // aria2_original advertises compressed HTTP responses only when
            // --http-accept-gzip is enabled. Keep the protocol client safe
            // by default; callers can opt in explicitly.
            accept_gzip: false,
            verify_tls: true,
            ca_cert_path: None,
        }
    }
}

pub struct HttpClient {
    inner: Client,
    options: HttpClientOptions,
}

/// Validation failures returned by the fallible request-builder header API.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HttpRequestBuilderError {
    #[error("invalid header name: {0}")]
    InvalidName(String),
    #[error("invalid header value: {0}")]
    InvalidValue(String),
}

/// Lazily install the `ring` crypto provider for rustls on first call.
///
/// Required when reqwest is built with the `rustls-no-provider` feature;
/// without a provider, `ClientBuilder::build()` will panic.
///
/// Note: `install_default()` returns Err if a provider is already installed
/// (e.g., by test helpers' `ensure_crypto_provider()`). That is fine — we
/// only need to ensure a provider is present, not that we installed it.
pub(crate) fn ensure_ring_provider() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

impl HttpClient {
    pub fn new(options: HttpClientOptions) -> Result<Self, String> {
        // Ensure the TLS crypto provider is installed before constructing
        // the reqwest Client (required for rustls-no-provider builds).
        ensure_ring_provider();
        let mut builder = ClientBuilder::new()
            .connect_timeout(options.connect_timeout)
            .timeout(options.timeout)
            .user_agent(&options.user_agent)
            .redirect(redirect::Policy::limited(options.max_redirects));

        builder = builder.gzip(options.accept_gzip);

        if !options.verify_tls {
            builder = builder.danger_accept_invalid_certs(true);
        }

        if let Some(ref ca_path) = options.ca_cert_path {
            match std::fs::read(ca_path) {
                Ok(cert_bytes) => {
                    let cert = Certificate::from_pem(&cert_bytes)
                        .map_err(|e| format!("Failed to load CA certificate: {}", e))?;
                    builder = builder.add_root_certificate(cert);
                }
                Err(e) => return Err(format!("Failed to read CA certificate file: {}", e)),
            }
        }

        let inner = builder
            .build()
            .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

        info!(
            "HttpClient initialized (timeout={:?}, max_redirects={}, verify_tls={})",
            options.timeout, options.max_redirects, options.verify_tls
        );

        Ok(Self { inner, options })
    }

    pub fn default_client() -> Result<Self, String> {
        Self::new(HttpClientOptions::default())
    }

    fn build_request(&self, request: HttpRequest) -> Result<reqwest::RequestBuilder, String> {
        debug!("Sending HTTP request: {} {}", request.method, request.url);

        let mut reqwest_request = match request.method.to_uppercase().as_str() {
            "GET" => self.inner.get(&request.url),
            "POST" => self.inner.post(&request.url),
            "HEAD" => self.inner.head(&request.url),
            "PUT" => self.inner.put(&request.url),
            _ => return Err(format!("Unsupported HTTP method: {}", request.method)),
        };

        if let Some(ref headers) = request.headers {
            for (key, value) in headers.iter() {
                reqwest_request = reqwest_request.header(
                    key.as_str()
                        .parse::<reqwest::header::HeaderName>()
                        .map_err(|e| format!("Invalid header name: {}", e))?,
                    value.as_str(),
                );
            }
        }

        if let Some(body) = request.body {
            reqwest_request = reqwest_request.body(body);
        }

        Ok(reqwest_request)
    }

    pub async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let response = self
            .build_request(request)?
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let (status_code, status_text, headers_map) = response_metadata(&response);
        debug!("Received HTTP response: status_code={}", status_code);

        let body_bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response body: {}", e))?
            .to_vec();

        Ok(HttpResponse {
            status_code,
            status_text,
            headers: headers_map,
            body: body_bytes,
        })
    }

    /// Execute a request without buffering its response body.
    ///
    /// The returned metadata is available after response headers arrive. Use
    /// [`HttpResponseStream::next_chunk`] for incremental reads, or drop the
    /// stream to cancel the remaining transfer.
    pub async fn execute_stream(&self, request: HttpRequest) -> Result<HttpResponseStream, String> {
        let response = self
            .build_request(request)?
            .send()
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        let (status_code, status_text, headers) = response_metadata(&response);
        debug!(
            "Received HTTP response headers: status_code={}",
            status_code
        );
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| format!("Failed to read response body: {}", e)))
            .boxed();

        Ok(HttpResponseStream {
            status_code,
            status_text,
            headers,
            body,
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

fn response_metadata(response: &reqwest::Response) -> (u16, String, Vec<(String, String)>) {
    let status = response.status();
    let headers = response
        .headers()
        .iter()
        .map(|(key, value)| {
            (
                key.as_str().to_string(),
                value.to_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    (
        status.as_u16(),
        status.canonical_reason().unwrap_or("Unknown").to_string(),
        headers,
    )
}

pub struct HttpRequestBuilder<'a> {
    client: &'a HttpClient,
    method: String,
    url: String,
    headers: Option<reqwest::header::HeaderMap>,
    body: Option<Vec<u8>>,
    error: Option<HttpRequestBuilderError>,
}

impl<'a> HttpRequestBuilder<'a> {
    fn new(client: &'a HttpClient, method: &str, url: String) -> Self {
        Self {
            client,
            method: method.to_string(),
            url,
            headers: None,
            body: None,
            error: None,
        }
    }

    /// Add a header using the existing chainable API.
    ///
    /// Invalid input is reported by [`Self::send`] or
    /// [`Self::send_stream`]. Use [`Self::try_header`] when validation should
    /// happen immediately.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        if let Err(error) = self.insert_header(name, value) {
            self.error = Some(error);
        }
        self
    }

    /// Add a header and return validation errors instead of panicking.
    ///
    /// `header` remains available for trusted, compile-time-known headers and
    /// preserves the existing chainable API. Use this method for values that
    /// originate outside the application, such as user configuration or an
    /// RPC request.
    pub fn try_header(mut self, name: &str, value: &str) -> Result<Self, HttpRequestBuilderError> {
        self.insert_header(name, value)?;
        Ok(self)
    }

    fn insert_header(&mut self, name: &str, value: &str) -> Result<(), HttpRequestBuilderError> {
        let mut headers = self.headers.take().unwrap_or_default();
        let name = name
            .parse::<reqwest::header::HeaderName>()
            .map_err(|error| HttpRequestBuilderError::InvalidName(error.to_string()))?;
        let value = value
            .parse::<reqwest::header::HeaderValue>()
            .map_err(|error| HttpRequestBuilderError::InvalidValue(error.to_string()))?;
        headers.insert(name, value);
        self.headers = Some(headers);
        Ok(())
    }

    /// Add a typed header using the existing chainable API.
    ///
    /// Conversion failures are reported by [`Self::send`] or
    /// [`Self::send_stream`]. Use [`Self::try_header_raw`] for immediate
    /// validation.
    pub fn header_raw<K, V>(mut self, key: K, value: V) -> Self
    where
        K: TryInto<reqwest::header::HeaderName>,
        V: TryInto<reqwest::header::HeaderValue>,
    {
        let mut headers = self.headers.take().unwrap_or_default();
        match (key.try_into(), value.try_into()) {
            (Ok(k), Ok(v)) => {
                headers.insert(k, v);
                self.headers = Some(headers);
            }
            (Err(_), _) => {
                self.error = Some(HttpRequestBuilderError::InvalidName(
                    "header name conversion failed".to_string(),
                ));
            }
            (_, Err(_)) => {
                self.error = Some(HttpRequestBuilderError::InvalidValue(
                    "header value conversion failed".to_string(),
                ));
            }
        }
        self
    }

    /// Add a typed header and preserve conversion errors for the caller.
    pub fn try_header_raw<K, V>(mut self, key: K, value: V) -> Result<Self, HttpRequestBuilderError>
    where
        K: TryInto<reqwest::header::HeaderName>,
        K::Error: std::fmt::Display,
        V: TryInto<reqwest::header::HeaderValue>,
        V::Error: std::fmt::Display,
    {
        let key = key
            .try_into()
            .map_err(|error| HttpRequestBuilderError::InvalidName(error.to_string()))?;
        let value = value
            .try_into()
            .map_err(|error| HttpRequestBuilderError::InvalidValue(error.to_string()))?;
        let mut headers = self.headers.take().unwrap_or_default();
        headers.insert(key, value);
        self.headers = Some(headers);
        Ok(self)
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

    fn into_request(self) -> Result<(&'a HttpClient, HttpRequest), HttpRequestBuilderError> {
        let Self {
            client,
            method,
            url,
            headers,
            body,
            error,
        } = self;
        if let Some(error) = error {
            return Err(error);
        }
        let headers_map = headers.map(|h| {
            h.iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>()
        });

        Ok((
            client,
            HttpRequest {
                method,
                url,
                headers: headers_map,
                body,
            },
        ))
    }

    pub async fn send(self) -> Result<HttpResponse, String> {
        let (client, request) = self.into_request().map_err(|error| error.to_string())?;
        client.execute(request).await
    }

    /// Send this request while keeping the response body incremental.
    pub async fn send_stream(self) -> Result<HttpResponseStream, String> {
        let (client, request) = self.into_request().map_err(|error| error.to_string())?;
        client.execute_stream(request).await
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
            debug!("Maximum redirect limit reached: {}", max_redirects);
            return None;
        }

        if !is_redirect_status(status_code) {
            return None;
        }

        match status_code {
            300 | 301 => Some(RedirectAction::FollowKeepMethod),
            302 | 303 => Some(RedirectAction::FollowChangeToGet),
            307 | 308 => Some(RedirectAction::FollowKeepMethod),
            _ => None,
        }
    }

    pub fn resolve_redirect_url(current_url: &str, location: &str) -> Result<String, String> {
        let location = location.trim();
        if location.is_empty() {
            return Err("Location header is empty".to_string());
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
            return Err(format!("Unsafe redirect URL protocol: {}", url));
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn default_options_do_not_advertise_gzip() {
        assert!(!HttpClientOptions::default().accept_gzip);
    }

    #[test]
    fn fallible_header_builder_reports_invalid_input() {
        let client = HttpClient::default_client().unwrap();

        let name_error = client
            .get("http://example.test")
            .try_header("invalid header name", "value");
        assert!(matches!(
            name_error,
            Err(HttpRequestBuilderError::InvalidName(_))
        ));

        let value_error = client
            .get("http://example.test")
            .try_header("X-Test", "invalid\nvalue");
        assert!(matches!(
            value_error,
            Err(HttpRequestBuilderError::InvalidValue(_))
        ));
    }

    #[tokio::test]
    async fn chainable_header_reports_invalid_input_without_panicking() {
        let client = HttpClient::default_client().unwrap();
        let result = client
            .get("http://example.test")
            .header("invalid header name", "value")
            .send()
            .await;

        assert!(matches!(
            result,
            Err(error) if error.contains("invalid header name")
        ));
    }

    #[test]
    fn test_should_follow_301() {
        let action = RedirectHandler::should_follow_redirect(301, "GET", 0, 5);
        assert!(action.is_some());

        let no_action = RedirectHandler::should_follow_redirect(301, "GET", 5, 5);
        assert!(no_action.is_none());
    }

    #[test]
    fn test_should_follow_300() {
        let action = RedirectHandler::should_follow_redirect(300, "GET", 0, 5);
        assert!(matches!(action, Some(RedirectAction::FollowKeepMethod)));
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

    #[tokio::test]
    async fn execute_stream_exposes_metadata_and_incremental_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = connection.read(&mut request).await.unwrap();
            connection
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nX-Test: stream\r\nConnection: close\r\n\r\nhello world",
                )
                .await
                .unwrap();
            connection.shutdown().await.unwrap();
        });

        let client = HttpClient::default_client().unwrap();
        let mut response = client
            .get(format!("http://{address}/payload"))
            .send_stream()
            .await
            .unwrap();
        assert_eq!(response.status_code(), 200);
        assert_eq!(response.status_text(), "OK");
        assert_eq!(response.header("x-test"), Some(&"stream".to_string()));

        let mut body = Vec::new();
        let mut chunks = 0;
        while let Some(chunk) = response.next_chunk().await {
            chunks += 1;
            body.extend_from_slice(&chunk.unwrap());
        }
        assert!(chunks > 0);
        assert_eq!(body, b"hello world");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn execute_stream_propagates_truncated_body_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = connection.read(&mut request).await.unwrap();
            connection
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nshort",
                )
                .await
                .unwrap();
            connection.shutdown().await.unwrap();
        });

        let client = HttpClient::default_client().unwrap();
        let mut response = client
            .execute_stream(HttpRequest::get(format!("http://{address}/truncated")))
            .await
            .unwrap();
        let mut saw_error = false;
        while let Some(chunk) = response.next_chunk().await {
            if chunk.is_err() {
                saw_error = true;
                break;
            }
        }
        assert!(
            saw_error,
            "premature EOF must not look like a successful body"
        );
        server.await.unwrap();
    }
}
