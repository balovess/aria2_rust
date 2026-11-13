use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tracing::debug;

use crate::crypto::ensure_ring_provider;

use super::{PublicTrackerList, TrackerEntry, TrackerSourceFetcher};

pub(super) async fn fetch_tracker_source(
    fetcher: &dyn TrackerSourceFetcher,
    source: &str,
    request_timeout: Duration,
) -> Result<Vec<TrackerEntry>, String> {
    debug!("Fetching public tracker list from {}", source);
    let body = tokio::time::timeout(request_timeout, fetcher.fetch(source, request_timeout))
        .await
        .map_err(|_| {
            format!(
                "Tracker source request timed out after {} seconds",
                request_timeout.as_secs_f64()
            )
        })??;
    let entries = PublicTrackerList::parse(&body);
    if entries.is_empty() {
        return Err("Parsed 0 entries from response".to_string());
    }
    Ok(entries)
}

pub(super) struct ReqwestTrackerSourceFetcher {
    client: reqwest::Client,
}

impl ReqwestTrackerSourceFetcher {
    pub(super) fn new() -> Self {
        ensure_ring_provider();
        let client = reqwest::Client::builder()
            .build()
            .expect("public tracker HTTP client should be constructible");
        Self { client }
    }
}

impl TrackerSourceFetcher for ReqwestTrackerSourceFetcher {
    fn fetch<'a>(
        &'a self,
        source: &'a str,
        _timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            let response = self
                .client
                .get(source)
                .send()
                .await
                .map_err(|error| format!("HTTP GET failed: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("HTTP error: {}", response.status()));
            }
            response
                .text()
                .await
                .map_err(|error| format!("Read body failed: {error}"))
        })
    }
}
