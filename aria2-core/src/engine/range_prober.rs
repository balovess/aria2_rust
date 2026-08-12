use std::sync::Arc;
use std::time::Duration;

use reqwest;

use crate::constants;
use crate::http::HttpRequestPolicy;

pub struct RangeProber {
    client: Arc<reqwest::Client>,
    cookie_header: Option<String>,
    request_policy: HttpRequestPolicy,
}

impl RangeProber {
    pub fn new(client: Arc<reqwest::Client>, request_policy: HttpRequestPolicy) -> Self {
        Self {
            client,
            cookie_header: None,
            request_policy,
        }
    }

    pub fn with_cookie_header(mut self, cookie_header: Option<String>) -> Self {
        self.cookie_header = cookie_header;
        self
    }

    pub async fn probe_range_support(&self, uri: &str, total_length: u64) -> bool {
        let probe_stage_1 = self.probe_single_range(uri, "bytes=0-0").await;
        tracing::debug!(
            "Range probe stage 1 (bytes=0-0) for {}: {}",
            uri,
            probe_stage_1
        );

        if !probe_stage_1 {
            tracing::info!(
                "Range probe stage 1 failed for {}, falling back to sequential",
                uri
            );
            return false;
        }

        let end_byte = if total_length > 1 {
            std::cmp::min(999, total_length - 1)
        } else {
            0
        };
        let range_header = format!("bytes=0-{}", end_byte);
        let probe_stage_2 = self.probe_single_range(uri, &range_header).await;
        tracing::debug!(
            "Range probe stage 2 ({}) for {}: {}",
            range_header,
            uri,
            probe_stage_2
        );

        if !probe_stage_2 {
            tracing::info!(
                "Range probe stage 2 failed for {} ({}), falling back to sequential",
                uri,
                range_header
            );
        }

        probe_stage_2
    }

    /// Fetch the entity length without using HEAD.
    ///
    /// A `bytes=0-0` GET is still a normal GET from the server's point of
    /// view, but a compliant range response exposes the complete entity size
    /// in `Content-Range`. This keeps the default download method compatible
    /// with aria2 while giving the Rust scheduler enough information to pick
    /// segmented downloading before it opens the full body.
    pub async fn probe_entity_length(&self, uri: &str) -> Option<(u64, bool)> {
        let req = self.request_policy.apply(
            self.client
                .get(uri)
                .header("Range", "bytes=0-0")
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                )),
            self.cookie_header.as_deref(),
            &[],
        );
        let response = req.send().await.ok()?;
        let supports_range = response.status().as_u16() == 206;
        let total_length = response
            .headers()
            .get("Content-Range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.rsplit_once('/'))
            .and_then(|(_, total)| total.parse::<u64>().ok())
            .or_else(|| response.content_length())?;
        Some((total_length, supports_range))
    }

    pub async fn probe_single_range(&self, uri: &str, range_header: &str) -> bool {
        let req = self.request_policy.apply(
            self.client
                .get(uri)
                .header("Range", range_header)
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                )),
            self.cookie_header.as_deref(),
            &[],
        );
        match req.send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                status == 206
            }
            Err(e) => {
                tracing::warn!("Range probe failed for {} ({}): {}", uri, range_header, e);
                false
            }
        }
    }
}
