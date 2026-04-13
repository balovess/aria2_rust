use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::active_output_registry::global_registry;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_protocol::metalink::parser::UrlEntry;

pub struct MetalinkDownloadCommand {
    group: Arc<tokio::sync::RwLock<RequestGroup>>,
    client: reqwest::Client,
    output_path: std::path::PathBuf,
    started: bool,
    completed: bool,
    completed_bytes: u64,
    metalink_data: Vec<u8>,
}

impl MetalinkDownloadCommand {
    pub fn new(
        gid: GroupId,
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes)
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("Metalink parse failed: {}", e)))
            })?;

        let file = doc.single_file().ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config(
                "Metalink contains multiple files or no files".into(),
            ))
        })?;

        if file.urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Metalink file has no download URLs".into(),
            )));
        }

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let filename = file.name.clone();
        let path = std::path::PathBuf::from(&dir).join(&filename);

        let urls: Vec<String> = file
            .get_sorted_urls()
            .iter()
            .map(|u| u.url.clone())
            .collect();
        let group = RequestGroup::new(gid, urls, options.clone());

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(300))
            .user_agent("aria2-rust/0.1.0")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!(
                    "HTTP client build failed: {}",
                    e
                )))
            })?;

        info!(
            "MetalinkDownloadCommand created: {} -> {} ({} mirrors)",
            file.name,
            path.display(),
            file.urls.len()
        );

        Ok(Self {
            group: Arc::new(tokio::sync::RwLock::new(group)),
            client,
            output_path: path,
            started: false,
            completed: false,
            completed_bytes: 0,
            metalink_data: metalink_bytes.to_vec(),
        })
    }

    pub async fn group(&self) -> tokio::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.read().await
    }
}

#[async_trait]
impl Command for MetalinkDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.write().await.start().await?;
            self.started = true;
        }

        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(&self.metalink_data)
            .map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("Metalink parse error: {}", e)))
            })?;

        let file = doc.single_file().ok_or_else(|| {
            Aria2Error::Fatal(FatalError::Config("No available file after parsing".into()))
        })?;

        let sorted_urls = file.get_sorted_urls();
        if sorted_urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "No download mirrors available".into(),
            )));
        }

        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        // Resolve filename collision against other active downloads.
        // If another task is already writing to self.output_path, a unique
        // name such as "file (1).ext" will be generated automatically.
        let resolved_output_path = global_registry().resolve(&self.output_path).await;

        // Helper closure to release the resolved path on every exit path.
        let release_path = |path: &std::path::Path| {
            let p = path.to_path_buf();
            // Best-effort async release; safe to drop the spawned future.
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::spawn(async move {
                global_registry().release(&p).await;
            });
        };

        let expected_size = file.size;
        let hash_entry = file.hashes.first().cloned();

        let mut last_error = None;

        for url_entry in &sorted_urls {
            debug!(
                "Trying mirror [priority={}] : {}",
                url_entry.priority, url_entry.url
            );

            match self.try_download_url(&url_entry.url, expected_size).await {
                Ok(data) => {
                    if let Some(ref hash) = hash_entry
                        && !self.verify_hash(&data, hash)?
                    {
                        warn!(
                            "Hash verification failed [{}]: trying next mirror",
                            hash.algo.as_standard_name()
                        );
                        last_error = Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!(
                                    "Hash verification failed: {}",
                                    hash.algo.as_standard_name()
                                ),
                            },
                        ));
                        continue;
                    }

                    let raw_writer = DefaultDiskWriter::new(&resolved_output_path);
                    let rate_limit = {
                        let g = self.group.read().await;
                        g.options().max_download_limit
                    };
                    let mut writer: Box<dyn DiskWriter> = match rate_limit {
                        Some(rate) if rate > 0 => Box::new(ThrottledWriter::new(
                            raw_writer,
                            RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)),
                        )),
                        _ => Box::new(raw_writer),
                    };
                    writer.write(&data).await?;
                    writer.finalize().await.ok();

                    self.completed_bytes = data.len() as u64;

                    {
                        let mut g = self.group.write().await;
                        g.update_progress(self.completed_bytes).await;
                        g.update_speed(self.completed_bytes, 0).await;
                        g.complete().await?;
                    }

                    info!(
                        "Metalink download done: {} ({} bytes from {})",
                        resolved_output_path.display(),
                        self.completed_bytes,
                        url_entry.url
                    );
                    self.completed = true;
                    release_path(&resolved_output_path);
                    return Ok(());
                }
                Err(e) => {
                    warn!("Mirror download failed {}: {}", url_entry.url, e);
                    last_error = Some(e);
                }
            }
        }

        release_path(&resolved_output_path);
        Err(last_error
            .unwrap_or_else(|| Aria2Error::Fatal(FatalError::Config("All mirrors failed".into()))))
    }

    fn status(&self) -> CommandStatus {
        if self.completed {
            CommandStatus::Completed
        } else if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

impl MetalinkDownloadCommand {
    async fn try_download_url(&mut self, url: &str, expected_size: Option<u64>) -> Result<Vec<u8>> {
        let response = self.client.get(url).send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP request failed: {}", e),
            })
        })?;

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if status.as_u16() >= 500 {
                return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                    code: status.as_u16(),
                }));
            }
            return Err(Aria2Error::Fatal(FatalError::Config(format!(
                "HTTP error: {}",
                status
            ))));
        }

        let total_length = response.content_length().unwrap_or(0) as u64;

        {
            let mut g = self.group.write().await;
            g.set_total_length(total_length.max(expected_size.unwrap_or(0)))
                .await;
        }

        let mut data = Vec::with_capacity(total_length as usize);
        let mut stream = response.bytes_stream();
        let _start_time = Instant::now();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        while let Some(chunk_result) = stream.next().await {
            let bytes: bytes::Bytes = chunk_result.map_err(|e: reqwest::Error| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;
            data.extend_from_slice(&bytes);
            self.completed_bytes = data.len() as u64;

            let elapsed = last_speed_update.elapsed();
            if elapsed.as_millis() >= 500 {
                let delta = self.completed_bytes - last_completed;
                let speed = (delta as f64 / elapsed.as_secs_f64()) as u64;
                let g = self.group.write().await;
                g.update_progress(self.completed_bytes).await;
                g.update_speed(speed, 0).await;
                last_speed_update = Instant::now();
                last_completed = self.completed_bytes;
            }
        }

        Ok(data)
    }

    fn verify_hash(
        &self,
        data: &[u8],
        hash: &aria2_protocol::metalink::parser::HashEntry,
    ) -> Result<bool> {
        use aria2_protocol::metalink::parser::HashAlgorithm;

        match hash.algo {
            HashAlgorithm::Md5 => {
                let digest = md5::compute(data);
                Ok(format!("{:x}", digest) == hash.value)
            }
            HashAlgorithm::Sha1 => {
                use sha1::Digest;
                let mut hasher = sha1::Sha1::new();
                hasher.update(data);
                let result = hasher.finalize();
                Ok(format!("{:x}", result) == hash.value)
            }
            HashAlgorithm::Sha256 => {
                use sha2::Digest;
                let mut hasher = sha2::Sha256::new();
                hasher.update(data);
                let result = hasher.finalize();
                Ok(format!("{:x}", result) == hash.value)
            }
            HashAlgorithm::Sha512 => {
                use sha2::Digest;
                let mut hasher = sha2::Sha512::new();
                hasher.update(data);
                let result = hasher.finalize();
                Ok(format!("{:x}", result) == hash.value)
            }
        }
    }
}

// =========================================================================
// K3.3 — Tests for Metalink Priority Ordering
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::metalink::parser::UrlEntry;

    /// Test K3.3 #1: Priority descending order works correctly.
    ///
    /// Verifies that URLs are sorted by priority in descending order
    /// (higher priority number = tried first).
    #[test]
    fn test_priority_descending_order() {
        let urls = vec![
            UrlEntry::new("http://mirror3.example.com/file.bin").with_priority(1),
            UrlEntry::new("http://mirror1.example.com/file.bin").with_priority(3),
            UrlEntry::new("http://mirror2.example.com/file.bin").with_priority(2),
        ];

        let sorted = select_mirrors_by_priority(&urls, "");

        // Should be ordered by priority descending: [3, 2, 1]
        assert_eq!(sorted.len(), 3, "Should return all URLs");
        assert_eq!(
            sorted[0].priority, 3,
            "First URL should have highest priority (3)"
        );
        assert_eq!(
            sorted[1].priority, 2,
            "Second URL should have medium priority (2)"
        );
        assert_eq!(
            sorted[2].priority, 1,
            "Third URL should have lowest priority (1)"
        );

        // Verify URL ordering matches priority
        assert!(
            sorted[0].url.contains("mirror1"),
            "First should be mirror1 (priority 3)"
        );
        assert!(
            sorted[1].url.contains("mirror2"),
            "Second should be mirror2 (priority 2)"
        );
        assert!(
            sorted[2].url.contains("mirror3"),
            "Third should be mirror3 (priority 1)"
        );
    }

    /// Test K3.3 #2: Location preference boosts matching URLs among same priority.
    ///
    /// When multiple URLs have the same priority, those matching the location
    /// preference should be tried first.
    #[test]
    fn test_location_preference_boosts_matching() {
        let urls = vec![
            UrlEntry::new("http://us-mirror1.example.com/file.bin")
                .with_priority(5)
                .with_location("us"),
            UrlEntry::new("http://eu-mirror1.example.com/file.bin")
                .with_priority(5)
                .with_location("eu"),
            UrlEntry::new("http://eu-mirror2.example.com/file.bin")
                .with_priority(5)
                .with_location("eu"),
            UrlEntry::new("http://jp-mirror1.example.com/file.bin")
                .with_priority(5)
                .with_location("jp"),
        ];

        // Prefer EU locations
        let sorted = select_mirrors_by_priority(&urls, "eu");

        assert_eq!(sorted.len(), 4, "Should return all URLs");

        // All have same priority (5), so EU ones should come first
        let eu_urls: Vec<_> = sorted
            .iter()
            .filter(|u| u.location.as_deref() == Some("eu"))
            .collect();

        assert_eq!(eu_urls.len(), 2, "Should find 2 EU mirrors");

        // EU mirrors should appear before non-EU mirrors
        let first_non_eu_idx = sorted
            .iter()
            .position(|u| u.location.as_deref() != Some("eu"))
            .expect("Should find at least one non-EU mirror");

        let last_eu_idx = sorted
            .iter()
            .rposition(|u| u.location.as_deref() == Some("eu"))
            .expect("Should find EU mirrors");

        assert!(
            last_eu_idx < first_non_eu_idx,
            "EU mirrors should come before non-EU mirrors"
        );

        // Test with US preference
        let sorted_us = select_mirrors_by_priority(&urls, "us");
        let us_first = &sorted_us[0];
        assert_eq!(
            us_first.location.as_deref(),
            Some("us"),
            "US mirror should be first when preferring US"
        );
    }

    /// Test K3.3 #3: Failover tries all mirrors then returns error when all fail.
    ///
    /// Verifies that try_mirrors_with_failover attempts every mirror and
    /// returns an error message when all attempts fail.
    #[tokio::test]
    async fn test_failover_tries_all_then_errors() {
        let urls = vec![
            UrlEntry::new("http://mirror1.fail/file.bin").with_priority(3),
            UrlEntry::new("http://mirror2.fail/file.bin").with_priority(2),
            UrlEntry::new("http://mirror3.fail/file.bin").with_priority(1),
        ];

        // Download function that always fails
        let fail_fn = |url: &str| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::result::Result<Vec<u8>, String>> + '_>,
        > {
            let url_owned = url.to_string();
            Box::pin(async move { Err(format!("Connection refused to {}", url_owned)) })
        };

        let url_refs: Vec<&UrlEntry> = urls.iter().collect();

        let result = try_mirrors_with_failover(&url_refs, fail_fn).await;

        assert!(result.is_err(), "Should return error when all mirrors fail");

        let error_msg = result.unwrap_err();
        assert!(
            error_msg.contains("All 3 mirrors failed"),
            "Error message should indicate all 3 mirrors failed"
        );
    }

    /// Test K3.3 #4: Single mirror succeeds immediately without failover.
    ///
    /// Verifies that when there's only one mirror and it succeeds,
    /// the data is returned without attempting additional failover.
    #[tokio::test]
    async fn test_single_mirror_no_failover_needed() {
        let urls =
            vec![UrlEntry::new("http://working-mirror.example.com/success.bin").with_priority(10)];

        let expected_data = b"Downloaded file content".to_vec();

        // Download function that succeeds immediately
        // Use Arc so data can be cloned multiple times (Fn trait requirement)
        let data_shared = std::sync::Arc::new(expected_data.clone());
        let success_fn = move |_url: &str| {
            let data = data_shared.clone();
            async move { Ok((*data).clone()) }
        };

        let result = try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &success_fn).await;

        assert!(result.is_ok(), "Single working mirror should succeed");

        let downloaded_data = result.unwrap();
        assert_eq!(
            downloaded_data, expected_data,
            "Downloaded data should match expected content"
        );
        assert_eq!(
            downloaded_data.len(),
            expected_data.len(),
            "Should download exactly {} bytes",
            expected_data.len()
        );
    }

    /// Additional test: Mixed priorities with location preference.
    ///
    /// Verifies that primary sort (priority) takes precedence over secondary
    /// sort (location). A higher-priority non-matching URL should still come
    /// before a lower-priority matching URL.
    #[test]
    fn test_priority_overrides_location() {
        let urls = vec![
            UrlEntry::new("http://low-eu.example.com/file.bin")
                .with_priority(1)
                .with_location("eu"), // Low priority but matches location
            UrlEntry::new("http://high-us.example.com/file.bin")
                .with_priority(10)
                .with_location("us"), // High priority but doesn't match
        ];

        let sorted = select_mirrors_by_priority(&urls, "eu");

        // High priority (10) should come first even though it doesn't match location
        assert_eq!(
            sorted[0].priority, 10,
            "Higher priority URL should come first regardless of location match"
        );
        assert_eq!(
            sorted[0].url, "http://high-us.example.com/file.bin",
            "First should be high-priority US URL"
        );
        assert_eq!(
            sorted[1].priority, 1,
            "Lower priority URL should come second"
        );
    }

    /// Additional test: Empty resource list returns empty result.
    #[test]
    fn test_empty_resources_returns_empty() {
        let urls: Vec<UrlEntry> = Vec::new();
        let sorted = select_mirrors_by_priority(&urls, "");

        assert!(sorted.is_empty(), "Empty input should produce empty output");
    }

    /// Additional test: Failover succeeds on second attempt after first fails.
    #[tokio::test]
    async fn test_failover_succeeds_on_second_mirror() {
        let urls = vec![
            UrlEntry::new("http://failing-mirror.example.com/file.bin").with_priority(5),
            UrlEntry::new("http://working-mirror.example.com/file.bin").with_priority(3),
        ];

        let attempt_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = attempt_count.clone();
        let fallback_fn = move |url: &str| {
            let url_owned = url.to_string();
            let count = count_clone.clone();
            async move {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if url_owned.contains("failing") {
                    Err("Connection timeout".to_string())
                } else {
                    Ok(b"Success data".to_vec())
                }
            }
        };

        let result =
            try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &fallback_fn).await;

        assert!(result.is_ok(), "Should succeed on second mirror");
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "Should have attempted 2 mirrors"
        );

        let data = result.unwrap();
        assert_eq!(
            data, b"Success data",
            "Data from second mirror should be returned"
        );
    }
}

// =========================================================================
// K3 — Metalink Priority Ordering Functions
// =========================================================================

/// Sort metalink URL resources by priority descending, then by location preference.
///
/// Higher priority number means tried first (priority 10 before priority 1).
/// Within same priority level, URLs matching the location preference are
/// preferred over non-matching ones.
///
/// # Arguments
///
/// * `resources` - Slice of UrlEntry resources to sort
/// * `location_preference` - Optional location code (e.g., "eu", "us", "jp")
///   to boost matching URLs within same priority level
///
/// # Returns
///
/// A vector of references sorted by:
/// 1. Priority descending (higher priority first)
/// 2. Location preference match (matching locations first among equal priority)
///
/// # Example
///
/// ```ignore
/// use aria2_core::engine::metalink_download_command::select_mirrors_by_priority;
/// use aria2_protocol::metalink::parser::UrlEntry;
///
/// let urls = vec![
///     UrlEntry::new("http://eu.example.com/file.bin").with_priority(2).with_location("eu"),
///     UrlEntry::new("http://us.example.com/file.bin").with_priority(3).with_location("us"),
/// ];
///
/// let sorted = select_mirrors_by_priority(&urls, "eu");
/// // Result: [us URL (priority 3), eu URL (priority 2)]
/// ```
pub fn select_mirrors_by_priority<'a>(
    resources: &'a [UrlEntry],
    location_preference: &str,
) -> Vec<&'a UrlEntry> {
    let mut sorted: Vec<&'a UrlEntry> = resources.iter().collect();

    sorted.sort_by(|a, b| {
        // Primary sort: priority descending (higher priority number = more preferred)
        let pri_cmp = b.priority.cmp(&a.priority);
        if pri_cmp != std::cmp::Ordering::Equal {
            return pri_cmp;
        }

        // Secondary sort: location preference (if specified and non-empty)
        if !location_preference.is_empty() {
            let a_matches = a
                .location
                .as_ref()
                .map(|l| {
                    l.contains(location_preference) || location_preference.contains(l.as_str())
                })
                .unwrap_or(false);
            let b_matches = b
                .location
                .as_ref()
                .map(|l| {
                    l.contains(location_preference) || location_preference.contains(l.as_str())
                })
                .unwrap_or(false);

            // Prefer matching location when priorities are equal
            if a_matches != b_matches {
                return b_matches.cmp(&a_matches);
            }
        }

        std::cmp::Ordering::Equal
    });

    sorted
}

/// Try mirrors in priority order until one succeeds or all fail.
///
/// Iterates through sorted URL entries attempting download with each.
/// Returns immediately on first success, or error after all attempts fail.
///
/// This implements automatic mirror failover for improved reliability:
/// - Logs each attempt with index and URL
/// - Continues to next mirror on failure
/// - Returns downloaded data from first successful mirror
/// - Aggregates errors if all mirrors fail
///
/// # Type Parameters
///
/// * `F` - Download function type that takes URL string and returns future
/// * `Fut` - Future type returned by download function
///
/// # Arguments
///
/// * `sorted_urls` - Slice of URL references in priority order (first = highest priority)
/// * `download_fn` - Async function that attempts download from a single URL
///
/// # Returns
///
/// * `Ok(Vec<u8>)` - Downloaded data from first successful mirror
/// * `Err(String)` - Error message if all mirrors failed
///
/// # Example
///
/// ```ignore
/// use aria2_core::engine::metalink_download_command::try_mirrors_with_failover;
///
/// async fn download(url: &str) -> Result<Vec<u8>, String> {
///     // HTTP GET implementation
/// }
///
/// let mirrors = vec![&url_entry1, &url_entry2];
/// match try_mirrors_with_failover(&mirrors, &download).await {
///     Ok(data) => println!("Downloaded {} bytes", data.len()),
///     Err(e) => println!("All mirrors failed: {}", e),
/// }
/// ```
pub async fn try_mirrors_with_failover<F, Fut>(
    sorted_urls: &[&UrlEntry],
    download_fn: F,
) -> std::result::Result<Vec<u8>, String>
where
    F: Fn(&str) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Vec<u8>, String>>,
{
    for (i, url_res) in sorted_urls.iter().enumerate() {
        info!(
            index = i,
            url = %url_res.url,
            priority = url_res.priority,
            "Trying mirror"
        );

        match download_fn(&url_res.url).await {
            Ok(data) => {
                info!(
                    index = i,
                    size = data.len(),
                    url = %url_res.url,
                    "Mirror succeeded"
                );
                return Ok(data);
            }
            Err(e) => {
                warn!(
                    index = i,
                    url = %url_res.url,
                    error = %e,
                    "Mirror failed, trying next"
                );
                continue;
            }
        }
    }

    Err(format!("All {} mirrors failed", sorted_urls.len()))
}
