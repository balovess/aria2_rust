use std::sync::Arc;

use crate::engine::download_cookie::CookieHelper;
use crate::engine::http_segment_downloader::{HttpSegmentDownloader, RangeProbeResult};
use crate::error::Result;
use crate::http::{AuthResolveOptions, HttpRequestPolicy};

/// Redirect-aware proof that the selected resource supports byte ranges.
///
/// HEAD metadata and `Accept-Ranges` are advisory only. The returned URL and
/// length come from the final response to `GET Range: bytes=0-0`, which is the
/// same redirect/auth/cookie path used by the actual segment requests.
pub struct RangeProber {
    downloader: HttpSegmentDownloader,
    cookie_header: Option<String>,
}

impl RangeProber {
    pub fn new(client: Arc<reqwest::Client>, request_policy: HttpRequestPolicy) -> Self {
        Self {
            downloader: HttpSegmentDownloader::new_with_policy(&client, request_policy),
            cookie_header: None,
        }
    }

    pub fn with_cookie_header(mut self, cookie_header: Option<String>) -> Self {
        self.cookie_header = cookie_header;
        self
    }

    pub fn with_cookie_helper(mut self, cookie_helper: CookieHelper) -> Self {
        self.downloader = self.downloader.with_cookie_helper(cookie_helper);
        self
    }

    pub fn with_auth_options(
        mut self,
        auth_options: AuthResolveOptions,
        netrc_path: Option<String>,
    ) -> Self {
        self.downloader = self.downloader.with_auth_options(auth_options, netrc_path);
        self
    }

    pub async fn probe(&self, uri: &str) -> Result<RangeProbeResult> {
        self.downloader
            .probe_range_metadata(uri, self.cookie_header.as_deref())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn follows_redirect_and_uses_final_range_metadata() {
        crate::http::client_pool::ensure_rustls_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server = tokio::spawn(async move {
            let (mut redirect_stream, _) = listener.accept().await.expect("accept redirect");
            let mut redirect_request = [0u8; 2048];
            let bytes = redirect_stream
                .read(&mut redirect_request)
                .await
                .expect("read redirect request");
            assert!(
                String::from_utf8_lossy(&redirect_request[..bytes])
                    .to_ascii_lowercase()
                    .contains("range: bytes=0-0")
            );
            redirect_stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: /cdn/file\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .expect("write redirect response");

            let (mut target_stream, _) = listener.accept().await.expect("accept target");
            let mut target_request = [0u8; 2048];
            let bytes = target_stream
                .read(&mut target_request)
                .await
                .expect("read target request");
            assert!(
                String::from_utf8_lossy(&target_request[..bytes])
                    .to_ascii_lowercase()
                    .contains("range: bytes=0-0")
            );
            target_stream
                .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/10485760\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx")
                .await
                .expect("write range response");
        });

        let client = Arc::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client should build"),
        );
        let probe = RangeProber::new(client, HttpRequestPolicy::default());
        let result = probe
            .probe(&format!("http://{addr}/entry"))
            .await
            .expect("redirected Range probe should succeed");

        assert!(result.supports_range);
        assert_eq!(result.total_length, 10 * 1024 * 1024);
        assert_eq!(result.effective_url, format!("http://{addr}/cdn/file"));
        assert_eq!(result.version, Some(reqwest::Version::HTTP_11));

        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("fixture should finish")
            .expect("fixture task should succeed");
    }

    #[tokio::test]
    async fn does_not_trust_accept_ranges_when_server_ignores_range() {
        crate::http::client_pool::ensure_rustls_provider();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nAccept-Ranges: bytes\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello")
                .await
                .expect("write response");
        });

        let client = Arc::new(
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("client should build"),
        );
        let probe = RangeProber::new(client, HttpRequestPolicy::default());
        let result = probe
            .probe(&format!("http://{addr}/file"))
            .await
            .expect("ignored Range request should produce probe metadata");

        assert!(!result.supports_range);
        assert_eq!(result.total_length, 5);

        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("fixture should finish")
            .expect("fixture task should succeed");
    }
}
