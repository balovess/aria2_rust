use std::sync::Arc;
use std::time::Duration;

use reqwest;

use crate::constants;

pub struct RangeProber {
    client: Arc<reqwest::Client>,
    headers: Vec<(String, String)>,
}

impl RangeProber {
    pub fn new(client: Arc<reqwest::Client>, headers: Vec<(String, String)>) -> Self {
        Self { client, headers }
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

    pub async fn probe_single_range(&self, uri: &str, range_header: &str) -> bool {
        let mut req =
            self.client
                .get(uri)
                .header("Range", range_header)
                .timeout(Duration::from_secs(
                    constants::HTTP_DEFAULT_OVERALL_TIMEOUT_SECS,
                ));
        for (name, value) in &self.headers {
            req = req.header(name, value);
        }
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
