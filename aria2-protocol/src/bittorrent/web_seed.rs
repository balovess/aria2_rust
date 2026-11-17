//! HTTP Range exchange for BitTorrent Web Seeds (BEP 17/19).

use std::time::Duration;

use futures::StreamExt;
use tracing::debug;

use crate::http::redirect::{RedirectOriginScope, is_sensitive_header, same_origin};
use crate::http::response::is_redirect_status;

/// Failure while requesting a byte range from a Web Seed.
#[derive(Debug, thiserror::Error)]
pub enum WebSeedRequestError {
    /// The Web Seed returned a status other than 200 or 206.
    #[error("Unexpected HTTP status {0} from web-seed")]
    HttpStatus(u16),
    /// The HTTP exchange failed or violated the requested range bounds.
    #[error("{0}")]
    Failure(String),
}

/// Executes Web Seed HTTP Range requests over a caller-configured HTTP client.
///
/// Core configures the client with its DNS, proxy, TLS, and source-address
/// policies. This type owns the BEP-specific Range request and response-body
/// rules while leaving those network policies with the engine.
pub struct WebSeedHttpClient {
    url: String,
    client: reqwest::Client,
    timeout: Duration,
    user_agent: String,
    headers: Vec<(String, String)>,
}

impl WebSeedHttpClient {
    /// Build a Web Seed request client from an engine-configured HTTP client.
    ///
    /// The supplied client must disable automatic redirects. This client
    /// follows redirects itself so request headers can be filtered whenever
    /// the redirect chain crosses an origin boundary.
    pub fn new(
        url: impl Into<String>,
        client: reqwest::Client,
        timeout: Duration,
        user_agent: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            client,
            timeout,
            user_agent: user_agent.into(),
            headers: Vec::new(),
        }
    }

    /// Set task-specific HTTP headers used by each Web Seed request.
    ///
    /// `Range`, `Host`, and `User-Agent` are managed by the Web Seed client.
    /// Sensitive headers are removed permanently after a cross-origin
    /// redirect; proxy authentication remains configured on the HTTP client.
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Request exactly the supplied byte range into `destination`.
    ///
    /// HTTP 200 and 206 responses are accepted for compatibility with Web
    /// Seeds that ignore Range. `on_response_status` runs as soon as headers
    /// arrive, before status validation or body reads; `on_body_chunk` runs
    /// after each received chunk, allowing the caller to update task progress.
    pub async fn download_range_into(
        &self,
        piece_index: u32,
        piece_offset: u64,
        destination: &mut [u8],
        on_response_status: impl FnOnce(u16),
        mut on_body_chunk: impl FnMut(usize),
    ) -> Result<usize, WebSeedRequestError> {
        let length = u64::try_from(destination.len()).map_err(|_| {
            WebSeedRequestError::Failure("requested WebSeed range is too large".to_string())
        })?;
        if length == 0 {
            return Ok(0);
        }

        let range_end = piece_offset.saturating_add(length - 1);
        let range_header = format!("bytes={piece_offset}-{range_end}");
        debug!(
            piece_index,
            offset = piece_offset,
            length,
            url = %crate::http::redirect::url_origin_for_logging(&self.url),
            range = %range_header,
            "Web-seed HTTP Range request"
        );

        let initial_url = reqwest::Url::parse(&self.url).map_err(|error| {
            WebSeedRequestError::Failure(format!("Invalid Web-seed URL: {error}"))
        })?;
        let mut current_url = initial_url.clone();
        let mut origin_scope = RedirectOriginScope::new(initial_url);
        let mut redirect_count = 0;
        let response = loop {
            let allow_sensitive_headers = origin_scope.allows_sensitive_headers(&current_url);
            let mut request = self
                .client
                .get(current_url.as_str())
                .header("Range", &range_header);
            let user_agent = self
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("User-Agent"))
                .map(|(_, value)| value.as_str())
                .unwrap_or(&self.user_agent);
            request = request.header("User-Agent", user_agent);
            for (name, value) in &self.headers {
                if name.eq_ignore_ascii_case("host")
                    || name.eq_ignore_ascii_case("range")
                    || name.eq_ignore_ascii_case("user-agent")
                    || (!allow_sensitive_headers && is_sensitive_header(name))
                {
                    continue;
                }
                request = request.header(name, value);
            }
            let response = tokio::time::timeout(self.timeout, request.send())
                .await
                .map_err(|_| {
                    WebSeedRequestError::Failure(format!(
                        "Web-seed response headers timed out after {} seconds",
                        self.timeout.as_secs()
                    ))
                })?
                .map_err(|error| {
                    WebSeedRequestError::Failure(format!("HTTP request failed: {error}"))
                })?;
            let status = response.status().as_u16();
            if !is_redirect_status(status) {
                break response;
            }
            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                break response;
            };
            if redirect_count >= 5 {
                return Err(WebSeedRequestError::Failure(
                    "Web-seed redirect limit exceeded (5)".to_string(),
                ));
            }
            let target = current_url.join(location).map_err(|error| {
                WebSeedRequestError::Failure(format!("Invalid Web-seed redirect URL: {error}"))
            })?;
            let target_has_credentials =
                !target.username().is_empty() || target.password().is_some();
            let credentials_changed = target.username() != current_url.username()
                || target.password() != current_url.password();
            if !matches!(target.scheme(), "http" | "https")
                || (target_has_credentials
                    && (!same_origin(&current_url, &target) || credentials_changed))
            {
                return Err(WebSeedRequestError::Failure(format!(
                    "Unsafe Web-seed redirect URL: {}",
                    target.scheme()
                )));
            }
            origin_scope.follow_redirect(&current_url, &target);
            current_url = target;
            redirect_count += 1;
            drop(response);
        };

        let status = response.status().as_u16();
        on_response_status(status);
        if status != 200 && status != 206 {
            return Err(WebSeedRequestError::HttpStatus(status));
        }

        let mut received = 0usize;
        let mut stream = response.bytes_stream();
        loop {
            let next_chunk = tokio::time::timeout(self.timeout, stream.next())
                .await
                .map_err(|_| {
                    WebSeedRequestError::Failure(format!(
                        "Web-seed response body stalled for {} seconds",
                        self.timeout.as_secs()
                    ))
                })?;
            let Some(chunk) = next_chunk else {
                break;
            };
            let chunk = chunk.map_err(|error| {
                WebSeedRequestError::Failure(format!("Failed to read response body: {error}"))
            })?;
            let next = received.checked_add(chunk.len()).ok_or_else(|| {
                WebSeedRequestError::Failure(format!(
                    "Web-seed response exceeded requested range: expected at most {length} bytes"
                ))
            })?;
            if next > destination.len() {
                return Err(WebSeedRequestError::Failure(format!(
                    "Web-seed response exceeded requested range: expected at most {length} bytes"
                )));
            }
            destination[received..next].copy_from_slice(&chunk);
            if !chunk.is_empty() {
                on_body_chunk(chunk.len());
            }
            received = next;
        }

        Ok(received)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(url: String) -> WebSeedHttpClient {
        crate::crypto::ensure_ring_provider();
        WebSeedHttpClient::new(
            url,
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            Duration::from_secs(2),
            "aria2-rust-test",
        )
    }

    #[tokio::test]
    async fn sends_range_and_accepts_partial_content() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/file")
            .match_header("range", "bytes=10-12")
            .match_header("user-agent", "aria2-rust-test")
            .with_status(206)
            .with_body("abc")
            .create_async()
            .await;
        let client = client(format!("{}/file", server.url()));
        let mut destination = [0; 3];
        let mut status = None;
        let mut received = 0;
        let length = client
            .download_range_into(
                4,
                10,
                &mut destination,
                |value| status = Some(value),
                |count| received += count,
            )
            .await
            .unwrap();

        assert_eq!(length, 3);
        assert_eq!(&destination, b"abc");
        assert_eq!(status, Some(206));
        assert_eq!(received, 3);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn accepts_full_content_and_rejects_oversized_body() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/file")
            .with_status(200)
            .with_body("abcdef")
            .create_async()
            .await;
        let client = client(format!("{}/file", server.url()));
        let mut destination = [0; 3];
        let mut status = None;
        let error = client
            .download_range_into(1, 0, &mut destination, |value| status = Some(value), |_| {})
            .await
            .unwrap_err();

        assert_eq!(status, Some(200));
        assert!(error.to_string().contains("exceeded requested range"));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn reports_http_status_before_rejecting_it() {
        let mut server = mockito::Server::new_async().await;
        let mock = server
            .mock("GET", "/file")
            .with_status(404)
            .create_async()
            .await;
        let client = client(format!("{}/file", server.url()));
        let mut status = None;
        let error = client
            .download_range_into(1, 0, &mut [0; 1], |value| status = Some(value), |_| {})
            .await
            .unwrap_err();

        assert_eq!(status, Some(404));
        assert!(matches!(error, WebSeedRequestError::HttpStatus(404)));
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn cross_origin_redirect_drops_sensitive_headers_and_keeps_public_headers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target = tokio::spawn(async move {
            let (mut connection, _) = target_listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = connection.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "redirect target request should finish headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            connection
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap().to_ascii_lowercase()
        });

        let source_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let source_address = source_listener.local_addr().unwrap();
        let source = tokio::spawn(async move {
            let (mut connection, _) = source_listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut chunk = [0_u8; 1024];
                let read = connection.read(&mut chunk).await.unwrap();
                assert_ne!(read, 0, "source request should finish headers");
                request.extend_from_slice(&chunk[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            connection
                .write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/payload\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            String::from_utf8(request).unwrap().to_ascii_lowercase()
        });

        let client = client(format!("http://{source_address}/redirect")).with_headers(vec![
            ("Authorization".into(), "Bearer origin-secret".into()),
            ("X-API-Key".into(), "api-secret".into()),
            ("Cookie".into(), "sid=origin-cookie".into()),
            ("X-Trace-ID".into(), "trace-123".into()),
        ]);
        let mut destination = [0; 2];
        let length = client
            .download_range_into(0, 0, &mut destination, |_| {}, |_| {})
            .await
            .unwrap();
        assert_eq!(length, 2);
        assert_eq!(&destination, b"ok");

        let target_request = target.await.unwrap();
        assert!(
            !target_request.contains("authorization:"),
            "{target_request}"
        );
        assert!(!target_request.contains("x-api-key:"), "{target_request}");
        assert!(!target_request.contains("cookie:"), "{target_request}");
        assert!(
            target_request.contains("x-trace-id: trace-123"),
            "{target_request}"
        );
        let source_request = source.await.unwrap();
        assert!(
            source_request.contains("authorization: bearer origin-secret"),
            "{source_request}"
        );
        assert!(
            source_request.contains("x-api-key: api-secret"),
            "{source_request}"
        );
        assert!(
            source_request.contains("cookie: sid=origin-cookie"),
            "{source_request}"
        );
    }

    #[tokio::test]
    async fn same_origin_redirect_keeps_sensitive_headers() {
        let mut server = mockito::Server::new_async().await;
        let redirect = server
            .mock("GET", "/file")
            .with_status(302)
            .with_header("location", "/payload")
            .create_async()
            .await;
        let target = server
            .mock("GET", "/payload")
            .match_header("authorization", "Bearer origin-secret")
            .match_header("x-api-key", "api-secret")
            .with_status(206)
            .with_body("ok")
            .create_async()
            .await;

        let client = client(format!("{}/file", server.url())).with_headers(vec![
            ("Authorization".into(), "Bearer origin-secret".into()),
            ("X-API-Key".into(), "api-secret".into()),
        ]);
        let mut destination = [0; 2];
        assert_eq!(
            client
                .download_range_into(0, 0, &mut destination, |_| {}, |_| {})
                .await
                .unwrap(),
            2
        );
        assert_eq!(&destination, b"ok");
        redirect.assert_async().await;
        target.assert_async().await;
    }
}
