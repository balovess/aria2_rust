//! HTTP Range exchange for BitTorrent Web Seeds (BEP 17/19).

use std::time::Duration;

use futures::StreamExt;
use tracing::debug;

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
}

impl WebSeedHttpClient {
    /// Build a Web Seed request client from an engine-configured HTTP client.
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
        }
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
            url = self.url,
            range = %range_header,
            "Web-seed HTTP Range request"
        );

        let response = tokio::time::timeout(
            self.timeout,
            self.client
                .get(&self.url)
                .header("Range", &range_header)
                .header("User-Agent", &self.user_agent)
                .send(),
        )
        .await
        .map_err(|_| {
            WebSeedRequestError::Failure(format!(
                "Web-seed response headers timed out after {} seconds",
                self.timeout.as_secs()
            ))
        })?
        .map_err(|error| WebSeedRequestError::Failure(format!("HTTP request failed: {error}")))?;

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
            reqwest::Client::new(),
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
}
