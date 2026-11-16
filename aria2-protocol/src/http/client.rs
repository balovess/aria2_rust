use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{Certificate, Client, ClientBuilder};
use tracing::{debug, info};

use crate::crypto::ensure_ring_provider;
use crate::http::redirect::{RedirectOriginScope, is_sensitive_header, url_origin_for_logging};
use crate::http::request::HttpRequest;
use crate::http::response::{HttpResponse, is_redirect_status};

mod redirect_handler;
mod request_builder;

pub use redirect_handler::{RedirectAction, RedirectHandler, RedirectPolicy};
pub use request_builder::{HttpRequestBuilder, HttpRequestBuilderError};

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

impl HttpClient {
    pub fn new(options: HttpClientOptions) -> Result<Self, String> {
        // Ensure the TLS crypto provider is installed before constructing
        // the reqwest Client (required for rustls-no-provider builds).
        ensure_ring_provider();
        let mut builder = ClientBuilder::new()
            .connect_timeout(options.connect_timeout)
            .timeout(options.timeout)
            .user_agent(&options.user_agent)
            .redirect(reqwest::redirect::Policy::none());

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

    fn build_request(&self, request: &HttpRequest) -> Result<reqwest::RequestBuilder, String> {
        debug!(
            "Sending HTTP request: {} {}",
            request.method,
            url_origin_for_logging(&request.url)
        );

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

        if let Some(body) = &request.body {
            reqwest_request = reqwest_request.body(body.clone());
        }

        Ok(reqwest_request)
    }

    async fn send_with_redirects(
        &self,
        mut request: HttpRequest,
    ) -> Result<reqwest::Response, String> {
        let initial_url = match url::Url::parse(&request.url) {
            Ok(url) => url,
            Err(_) => {
                return self
                    .build_request(&request)?
                    .send()
                    .await
                    .map_err(|error| format!("HTTP request failed: {error}"));
            }
        };
        let mut current_url = initial_url.clone();
        let mut origin_scope = RedirectOriginScope::new(initial_url);
        let mut redirect_count = 0;

        loop {
            request.url = current_url.to_string();
            let response = self
                .build_request(&request)?
                .send()
                .await
                .map_err(|error| format!("HTTP request failed: {error}"))?;
            let status_code = response.status().as_u16();
            if !is_redirect_status(status_code) {
                return Ok(response);
            }

            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                return Ok(response);
            };
            let Some(action) = RedirectHandler::should_follow_redirect(
                status_code,
                &request.method,
                redirect_count,
                self.options.max_redirects,
            ) else {
                return Err(format!(
                    "HTTP redirect limit exceeded ({})",
                    self.options.max_redirects
                ));
            };

            let target_url = RedirectHandler::resolve_redirect_url(current_url.as_str(), location)?;
            let target_url = url::Url::parse(&target_url)
                .map_err(|error| format!("Invalid redirect URL: {error}"))?;
            RedirectHandler::sanitize_redirect_url(target_url.as_str())?;
            origin_scope.follow_redirect(&current_url, &target_url);
            if !origin_scope.allows_sensitive_headers(&target_url)
                && let Some(headers) = request.headers.as_mut()
            {
                headers.retain(|(name, _)| !is_sensitive_header(name));
            }

            let referer = redirect_referer(&current_url, &target_url);
            let headers = request.headers.get_or_insert_with(Vec::new);
            headers.retain(|(name, _)| {
                !name.eq_ignore_ascii_case("Host")
                    && !name.eq_ignore_ascii_case("Referer")
                    && (action != RedirectAction::FollowChangeToGet || !is_entity_header(name))
            });
            if let Some(referer) = referer {
                headers.push(("Referer".to_string(), referer));
            }

            if action == RedirectAction::FollowChangeToGet {
                request.method = "GET".to_string();
                request.body = None;
            }
            current_url = target_url;
            redirect_count += 1;
            drop(response);
        }
    }

    pub async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, String> {
        let response = self.send_with_redirects(request).await?;

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
        let response = self.send_with_redirects(request).await?;

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

fn redirect_referer(current: &url::Url, target: &url::Url) -> Option<String> {
    if current.scheme() == "https" && target.scheme() == "http" {
        return None;
    }
    let mut referer = current.clone();
    referer.set_username("").ok()?;
    referer.set_password(None).ok()?;
    referer.set_fragment(None);
    Some(referer.to_string())
}

fn is_entity_header(name: &str) -> bool {
    [
        "Content-Encoding",
        "Content-Length",
        "Content-Type",
        "Transfer-Encoding",
    ]
    .iter()
    .any(|header| name.eq_ignore_ascii_case(header))
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

#[cfg(test)]
mod tests;
