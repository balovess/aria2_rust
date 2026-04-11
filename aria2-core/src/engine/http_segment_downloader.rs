use futures::StreamExt;
use std::time::Duration;
use tracing::{debug, warn};

use crate::error::{Aria2Error, RecoverableError, Result};

pub struct HttpSegmentDownloader {
    client: reqwest::Client,
}

impl HttpSegmentDownloader {
    pub fn new(client: &reqwest::Client) -> Self {
        Self {
            client: client.clone(),
        }
    }

    pub async fn supports_range(&self, url: &str, cookie_header: Option<&str>) -> Result<bool> {
        let mut req = self.client.head(url);
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
        }
        let resp = req.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HEAD request failed: {}", e),
            })
        })?;

        if let Some(accept_ranges) = resp.headers().get("Accept-Ranges")
            && let Ok(value) = accept_ranges.to_str()
        {
            return Ok(value.to_lowercase().contains("bytes"));
        }

        let status = resp.status();
        if status.as_u16() >= 400 {
            return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                code: status.as_u16(),
            }));
        }

        Ok(false)
    }

    pub async fn download_range(
        &self,
        url: &str,
        offset: u64,
        length: u64,
        cookie_header: Option<&str>,
    ) -> Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }

        let range_header = format!("bytes={}-{}", offset, offset + length.saturating_sub(1));
        debug!("HTTP Range request: {} ({})", range_header, url);

        let mut req = self
            .client
            .get(url)
            .header("Range", &range_header)
            .timeout(Duration::from_secs(120));
        if let Some(ch) = cookie_header {
            req = req.header("Cookie", ch);
        }
        let response = req.send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP Range request failed: {}", e),
            })
        })?;

        let status = response.status();
        match status.as_u16() {
            206 => {}
            200 => {
                warn!(
                    "Server returned 200 instead of 206 for Range request (offset={}, len={}), reading full body",
                    offset, length
                );
            }
            416 => {
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!(
                            "Range not satisfiable: bytes={}-{}",
                            offset,
                            offset + length.saturating_sub(1)
                        ),
                    },
                ));
            }
            code if (400..500).contains(&code) => {
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("HTTP client error {}: {}", code, url),
                )));
            }
            code if code >= 500 => {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code,
                }));
            }
            _ => {}
        }

        let mut data = Vec::with_capacity(length as usize);
        let mut stream = response.bytes_stream();

        while let Some(chunk_result) = stream.next().await {
            match chunk_result {
                Ok(bytes) => data.extend_from_slice(&bytes),
                Err(e) => {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: format!("Stream read error: {}", e),
                        },
                    ));
                }
            }
        }

        if data.is_empty() && length > 0 {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Empty response for range {}-{} from {}",
                        offset,
                        offset + length.saturating_sub(1),
                        url
                    ),
                },
            ));
        }

        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_supports_range_no_server() {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl
            .supports_range("http://127.0.0.1:1/nonexistent", None)
            .await;
        assert!(result.is_err(), "should fail for unreachable host");
    }

    #[tokio::test]
    async fn test_download_range_zero_length() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let result = dl.download_range("http://example.com", 0, 0, None).await;
        assert!(result.is_ok(), "zero-length range should return empty vec");
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_downloader_creation() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);
        let _dl2 = HttpSegmentDownloader::new(&dl.client);
    }

    #[tokio::test]
    async fn test_download_range_with_mock_http_416() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 2048];
            stream.read(&mut buf).await.unwrap();
            stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        let url = format!("http://{}", addr);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let dl = HttpSegmentDownloader::new(&client);

        let result = dl.download_range(&url, 99999, 100, None).await;
        assert!(result.is_err(), "416 should be an error");

        server_handle.await.ok();
    }

    #[tokio::test]
    async fn test_supports_range_header_parsing() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);

        assert!(
            dl.supports_range(
                "http://invalid-host-name-that-does-not-exist-12345.com/",
                None
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn test_download_range_status_code_handling() {
        let client = reqwest::Client::new();
        let dl = HttpSegmentDownloader::new(&client);

        let result_404 = dl
            .download_range("http://httpbin.org/status/404", 0, 100, None)
            .await;
        assert!(result_404.is_err(), "404 should be fatal error");
    }
}
