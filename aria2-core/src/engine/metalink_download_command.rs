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
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::metalink::parser::UrlEntry;

/// Information about a single file download created from a multi-file Metalink.
///
/// Returned by [`MetalinkDownloadCommand::create_multi_file`] so the caller
/// can track each per-file command independently.
pub struct MetalinkFileInfo {
    /// The download command for this file.
    pub command: MetalinkDownloadCommand,
    /// The original file index in the Metalink document (0-based).
    pub file_index: usize,
}

pub struct MetalinkDownloadCommand {
    group: Arc<std::sync::RwLock<RequestGroup>>,
    client: reqwest::Client,
    output_path: std::path::PathBuf,
    started: bool,
    completed: bool,
    completed_bytes: u64,
    /// Raw Metalink data for re-parsing during execute().
    /// Only used for single-file mode. Empty in multi-file mode
    /// (each per-file command stores only its own file's data).
    metalink_data: Vec<u8>,
    /// Parsed file info for per-file mode (set by create_multi_file).
    /// When present, execute() uses this instead of re-parsing metalink_data.
    file_info: Option<FileDownloadInfo>,
}

/// Parsed file information used by per-file command instances
/// created by `create_multi_file()`.
struct FileDownloadInfo {
    /// Sorted URL entries for this file.
    sorted_urls: Vec<UrlEntry>,
    /// Expected file size (from Metalink).
    expected_size: Option<u64>,
    /// First hash entry for verification.
    hash_entry: Option<aria2_protocol::metalink::parser::HashEntry>,
}

impl MetalinkDownloadCommand {
    /// Create a `MetalinkDownloadCommand` for a single-file Metalink.
    ///
    /// For multi-file Metalinks, use [`create_multi_file`] instead, which
    /// returns one command per file. This method also works for multi-file
    /// Metalinks: it picks the first file and downloads it.
    ///
    /// # Arguments
    ///
    /// * `gid` - Group ID for the download
    /// * `metalink_bytes` - Raw Metalink XML data
    /// * `options` - Download options
    /// * `output_dir` - Override output directory (takes precedence over `options.dir`)
    pub fn new(
        gid: GroupId,
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
    ) -> Result<Self> {
        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes, None)
            .map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Metalink parse failed: {}", e)))
        })?;

        if doc.files.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Metalink contains no files".into(),
            )));
        }

        // Use the first file (single-file mode). For multi-file Metalinks,
        // the caller should use create_multi_file() instead.
        let file = &doc.files[0];

        if file.urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Metalink file has no download URL".into(),
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

        let client = build_http_client()?;

        if doc.files.len() > 1 {
            info!(
                "MetalinkDownloadCommand created for first file of {}: {} -> {} ({} mirrors) \
                 [use create_multi_file() for all files]",
                doc.files.len(),
                file.name,
                path.display(),
                file.urls.len()
            );
        } else {
            info!(
                "MetalinkDownloadCommand created: {} -> {} ({} mirrors)",
                file.name,
                path.display(),
                file.urls.len()
            );
        }

        Ok(Self {
            group: Arc::new(std::sync::RwLock::new(group)),
            client,
            output_path: path,
            started: false,
            completed: false,
            completed_bytes: 0,
            metalink_data: metalink_bytes.to_vec(),
            file_info: None,
        })
    }

    /// Create one `MetalinkDownloadCommand` per file in a multi-file Metalink.
    ///
    /// This is the Rust equivalent of C++ `Metalink2RequestGroup::createRequestGroup()`
    /// which creates one `RequestGroup` per `MetalinkEntry`. Each command
    /// downloads exactly one file from the Metalink using its own mirror list.
    ///
    /// # Arguments
    ///
    /// * `metalink_bytes` - Raw Metalink XML data
    /// * `options` - Download options (shared by all files)
    /// * `output_dir` - Override output directory
    /// * `gid_start` - Starting GID; each file gets `gid_start + i`
    ///
    /// # Returns
    ///
    /// A vector of `MetalinkFileInfo`, one per file with URLs in the Metalink.
    /// Files with no download URLs are skipped.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let commands = MetalinkDownloadCommand::create_multi_file(
    ///     &metalink_xml, &options, None, 100
    /// )?;
    /// for info in commands {
    ///     println!("File {}: {}", info.file_index, info.command.output_path.display());
    /// }
    /// ```
    pub fn create_multi_file(
        metalink_bytes: &[u8],
        options: &DownloadOptions,
        output_dir: Option<&str>,
        gid_start: u64,
    ) -> Result<Vec<MetalinkFileInfo>> {
        let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(metalink_bytes, None)
            .map_err(|e| {
            Aria2Error::Fatal(FatalError::Config(format!("Metalink parse failed: {}", e)))
        })?;

        if doc.files.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Metalink contains no files".into(),
            )));
        }

        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let client = build_http_client()?;

        let mut commands = Vec::with_capacity(doc.files.len());

        for (i, file) in doc.files.iter().enumerate() {
            if file.urls.is_empty() {
                debug!(
                    index = i,
                    name = %file.name,
                    "Skipping Metalink file with no URLs"
                );
                continue;
            }

            let gid = GroupId::new(gid_start + i as u64);
            let path = std::path::PathBuf::from(&dir).join(&file.name);

            let sorted_urls: Vec<UrlEntry> = file
                .get_sorted_urls()
                .iter()
                .map(|u| (*u).clone())
                .collect();
            let urls: Vec<String> = sorted_urls.iter().map(|u| u.url.clone()).collect();
            let group = RequestGroup::new(gid, urls, options.clone());

            let file_info = FileDownloadInfo {
                expected_size: file.size,
                hash_entry: file.strongest_hash().cloned(),
                sorted_urls,
            };

            info!(
                gid = gid.value(),
                index = i,
                name = %file.name,
                path = %path.display(),
                mirrors = file.urls.len(),
                "Created MetalinkDownloadCommand for file"
            );

            commands.push(MetalinkFileInfo {
                command: Self {
                    group: Arc::new(std::sync::RwLock::new(group)),
                    client: client.clone(),
                    output_path: path,
                    started: false,
                    completed: false,
                    completed_bytes: 0,
                    metalink_data: Vec::new(),
                    file_info: Some(file_info),
                },
                file_index: i,
            });
        }

        Ok(commands)
    }

    /// Create a `MetalinkDownloadCommand` from a pre-parsed, pre-sorted
    /// `MetalinkFile` entry.
    ///
    /// This is used by `MetalinkToRequestGroup` which handles its own
    /// parsing, filtering, and priority reordering. Unlike `create_multi_file`,
    /// this method does not re-parse the XML.
    ///
    /// # Arguments
    ///
    /// * `file` - Pre-parsed Metalink file entry with sorted URLs
    /// * `options` - Download options
    /// * `output_dir` - Override output directory
    /// * `gid_start` - Starting GID for this command
    pub fn create_multi_file_for_single(
        file: &aria2_protocol::metalink::parser::MetalinkFile,
        options: &DownloadOptions,
        output_dir: Option<&str>,
        gid_start: u64,
    ) -> Result<Vec<MetalinkFileInfo>> {
        let dir = output_dir
            .map(|d| d.to_string())
            .or_else(|| options.dir.clone())
            .unwrap_or_else(|| ".".to_string());

        let client = build_http_client()?;

        if file.urls.is_empty() {
            return Ok(Vec::new());
        }

        let gid = GroupId::new(gid_start);
        let path = std::path::PathBuf::from(&dir).join(&file.name);

        let sorted_urls: Vec<UrlEntry> = file
            .get_sorted_urls()
            .iter()
            .map(|u| (*u).clone())
            .collect();

        // Collect only non-P2P URLs (HTTP, HTTPS, FTP) for the URI list.
        // BitTorrent URLs go through a separate dependency mechanism.
        // Mirrors C++ AccumulateNonP2PUri.
        let urls: Vec<String> = sorted_urls
            .iter()
            .filter(|u| u.is_non_p2p())
            .map(|u| u.url.clone())
            .collect();

        if urls.is_empty() {
            debug!(
                name = %file.name,
                "Skipping Metalink file with no non-P2P URLs"
            );
            return Ok(Vec::new());
        }

        let group = RequestGroup::new(gid, urls, options.clone());

        let file_info = FileDownloadInfo {
            expected_size: file.size,
            hash_entry: file.strongest_hash().cloned(),
            sorted_urls,
        };

        info!(
            gid = gid.value(),
            name = %file.name,
            path = %path.display(),
            mirrors = file.urls.len(),
            "Created MetalinkDownloadCommand for single file"
        );

        Ok(vec![MetalinkFileInfo {
            command: Self {
                group: Arc::new(std::sync::RwLock::new(group)),
                client,
                output_path: path,
                started: false,
                completed: false,
                completed_bytes: 0,
                metalink_data: Vec::new(),
                file_info: Some(file_info),
            },
            file_index: 0,
        }])
    }

    /// Get the output path for this download.
    pub fn output_path(&self) -> &std::path::Path {
        &self.output_path
    }

    pub fn group(&self) -> std::sync::RwLockReadGuard<'_, RequestGroup> {
        self.group.recover()
    }

    /// Consume this command and return the inner `RequestGroup` Arc.
    ///
    /// Used by post-download handlers that need to extract the group
    /// for insertion into the reserved queue without cloning.
    pub fn into_group(self) -> Arc<std::sync::RwLock<RequestGroup>> {
        self.group
    }
}

/// Build the shared HTTP client for Metalink downloads.
fn build_http_client() -> Result<reqwest::Client> {
    crate::http::client_pool::ensure_rustls_provider();
    reqwest::Client::builder()
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
        })
}

#[async_trait]
impl Command for MetalinkDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        // Resolve file info: either from pre-parsed file_info (multi-file mode)
        // or by re-parsing the raw metalink_data (single-file mode).
        // We extract owned data to avoid lifetime/borrow issues.
        let sorted_urls_owned: Vec<UrlEntry>;
        let expected_size: Option<u64>;
        let hash_entry_owned: Option<aria2_protocol::metalink::parser::HashEntry>;

        match &self.file_info {
            Some(info) => {
                sorted_urls_owned = info.sorted_urls.clone();
                expected_size = info.expected_size;
                hash_entry_owned = info.hash_entry.clone();
            }
            None => {
                let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(
                    &self.metalink_data,
                    None,
                )
                .map_err(|e| {
                    Aria2Error::Fatal(FatalError::Config(format!("Metalink parse error: {}", e)))
                })?;

                let file = if doc.files.len() == 1 {
                    &doc.files[0]
                } else {
                    // Multi-file Metalink in single-file mode: use first file
                    &doc.files[0]
                };

                sorted_urls_owned = file
                    .get_sorted_urls()
                    .iter()
                    .map(|u| (*u).clone())
                    .collect();
                expected_size = file.size;
                hash_entry_owned = file.hashes.first().cloned();

                if sorted_urls_owned.is_empty() {
                    return Err(Aria2Error::Fatal(FatalError::Config(
                        "No download mirrors available".into(),
                    )));
                }
            }
        }

        if sorted_urls_owned.is_empty() {
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

        let mut last_error = None;

        for url_entry in &sorted_urls_owned {
            debug!(
                "Trying mirror [priority={}] : {}",
                url_entry.priority, url_entry.url
            );

            match self.try_download_url(&url_entry.url, expected_size).await {
                Ok(data) => {
                    if let Some(ref hash) = hash_entry_owned
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
                        let g = self.group.recover();
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
                        let g = self.group.recover();
                        g.update_progress(self.completed_bytes);
                        g.update_speed(self.completed_bytes, 0);
                        drop(g);
                        let mut g = self.group.recover_mut();
                        g.complete()?;
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

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
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

        // Read Content-Length from the header directly instead of using
        // response.content_length(), which returns the *body* size. For chunked
        // transfer encoding or proxy-modified responses the body size may differ
        // from the advertised header value. The header value is what the server
        // advertised and is consistent with download_command.rs's approach.
        let total_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);

        {
            let g = self.group.recover();
            g.set_total_length(total_length.max(expected_size.unwrap_or(0)));
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
                let g = self.group.recover();
                g.update_progress(self.completed_bytes);
                g.update_speed(speed, 0);
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
                use md5::Digest;
                let mut hasher = md5::Md5::new();
                hasher.update(data);
                let digest = hasher.finalize();
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
// K3 — Metalink Priority Ordering Functions
// =========================================================================

/// Sort metalink URL resources by priority ascending, then by location preference.
///
/// Lower priority number means tried first (priority 1 before priority 10),
/// matching the C++ `MetalinkEntry::reorderResourcesByPriority()` which uses
/// `PriorityHigher` comparator: `res1->priority < res2->priority` (ascending).
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
/// 1. Priority ascending (lower priority number = tried first)
/// 2. Location preference match (matching locations first among equal priority)
pub fn select_mirrors_by_priority<'a>(
    resources: &'a [UrlEntry],
    location_preference: &str,
) -> Vec<&'a UrlEntry> {
    let mut sorted: Vec<&'a UrlEntry> = resources.iter().collect();

    sorted.sort_by(|a, b| {
        // Primary sort: priority ascending (lower priority number = more preferred)
        // Matches C++ PriorityHigher: res1->priority < res2->priority
        let pri_cmp = a.priority.cmp(&b.priority);
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

// =========================================================================
// Tests
// =========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use aria2_protocol::metalink::parser::UrlEntry;

    #[test]
    fn test_priority_ascending_order() {
        let urls = vec![
            UrlEntry::new("http://mirror3.example.com/file.bin").with_priority(3),
            UrlEntry::new("http://mirror1.example.com/file.bin").with_priority(1),
            UrlEntry::new("http://mirror2.example.com/file.bin").with_priority(2),
        ];

        let sorted = select_mirrors_by_priority(&urls, "");

        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0].priority, 1);
        assert_eq!(sorted[1].priority, 2);
        assert_eq!(sorted[2].priority, 3);
    }

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

        let sorted = select_mirrors_by_priority(&urls, "eu");

        assert_eq!(sorted.len(), 4);

        // EU mirrors should appear before non-EU mirrors
        let first_non_eu_idx = sorted
            .iter()
            .position(|u| u.location.as_deref() != Some("eu"))
            .expect("Should find at least one non-EU mirror");

        let last_eu_idx = sorted
            .iter()
            .rposition(|u| u.location.as_deref() == Some("eu"))
            .expect("Should find EU mirrors");

        assert!(last_eu_idx < first_non_eu_idx);
    }

    #[tokio::test]
    async fn test_failover_tries_all_then_errors() {
        let urls = [
            UrlEntry::new("http://mirror1.fail/file.bin").with_priority(3),
            UrlEntry::new("http://mirror2.fail/file.bin").with_priority(2),
            UrlEntry::new("http://mirror3.fail/file.bin").with_priority(1),
        ];

        let fail_fn = |url: &str| -> std::pin::Pin<
            Box<dyn std::future::Future<Output = std::result::Result<Vec<u8>, String>> + '_>,
        > {
            let url_owned = url.to_string();
            Box::pin(async move { Err(format!("Connection refused to {}", url_owned)) })
        };

        let url_refs: Vec<&UrlEntry> = urls.iter().collect();
        let result = try_mirrors_with_failover(&url_refs, fail_fn).await;

        assert!(result.is_err());
        assert!(result.unwrap_err().contains("All 3 mirrors failed"));
    }

    #[tokio::test]
    async fn test_single_mirror_no_failover_needed() {
        let urls =
            [UrlEntry::new("http://working-mirror.example.com/success.bin").with_priority(10)];

        let expected_data = b"Downloaded file content".to_vec();
        let data_shared = std::sync::Arc::new(expected_data.clone());
        let success_fn = move |_url: &str| {
            let data = data_shared.clone();
            async move { Ok((*data).clone()) }
        };

        let result = try_mirrors_with_failover(&urls.iter().collect::<Vec<_>>(), &success_fn).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), expected_data);
    }

    #[test]
    fn test_priority_overrides_location() {
        let urls = vec![
            UrlEntry::new("http://high-eu.example.com/file.bin")
                .with_priority(10)
                .with_location("eu"),
            UrlEntry::new("http://low-us.example.com/file.bin")
                .with_priority(1)
                .with_location("us"),
        ];

        let sorted = select_mirrors_by_priority(&urls, "eu");
        assert_eq!(sorted[0].priority, 1);
        assert_eq!(sorted[1].priority, 10);
    }

    #[test]
    fn test_empty_resources_returns_empty() {
        let urls: Vec<UrlEntry> = Vec::new();
        let sorted = select_mirrors_by_priority(&urls, "");
        assert!(sorted.is_empty());
    }

    #[tokio::test]
    async fn test_failover_succeeds_on_second_mirror() {
        let urls = [
            UrlEntry::new("http://failing-mirror.example.com/file.bin").with_priority(1),
            UrlEntry::new("http://working-mirror.example.com/file.bin").with_priority(2),
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

        assert!(result.is_ok());
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(result.unwrap(), b"Success data");
    }

    // ── Multi-file Metalink tests ─────────────────────────────────────────

    fn make_multi_file_xml() -> Vec<u8> {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="first.bin">
      <size>1024</size>
      <hash type="sha-256">aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa</hash>
      <url priority="1">http://mirror.example.com/first.bin</url>
    </file>
    <file name="second.bin">
      <size>2048</size>
      <hash type="sha-256">bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb</hash>
      <url priority="1">http://mirror.example.com/second.bin</url>
    </file>
  </files>
</metalink>"#
            .as_bytes()
            .to_vec()
    }

    #[test]
    fn test_new_accepts_multi_file_metalink() {
        let options = DownloadOptions::default();
        // Previously this would return "Metalink contains multiple files or no files"
        // Now it should succeed, picking the first file
        let result =
            MetalinkDownloadCommand::new(GroupId::new(1), &make_multi_file_xml(), &options, None);
        assert!(result.is_ok(), "new() should accept multi-file Metalink");
    }

    #[test]
    fn test_create_multi_file_returns_all_files() {
        let options = DownloadOptions::default();
        let commands =
            MetalinkDownloadCommand::create_multi_file(&make_multi_file_xml(), &options, None, 100)
                .unwrap();

        assert_eq!(commands.len(), 2, "Should create 2 commands for 2 files");
        assert_eq!(commands[0].file_index, 0);
        assert_eq!(commands[1].file_index, 1);
        assert!(
            commands[0]
                .command
                .output_path
                .to_string_lossy()
                .contains("first.bin"),
            "First command should be for first.bin"
        );
        assert!(
            commands[1]
                .command
                .output_path
                .to_string_lossy()
                .contains("second.bin"),
            "Second command should be for second.bin"
        );
    }

    #[test]
    fn test_create_multi_file_assigns_incrementing_gids() {
        let options = DownloadOptions::default();
        let commands =
            MetalinkDownloadCommand::create_multi_file(&make_multi_file_xml(), &options, None, 200)
                .unwrap();

        let g0 = commands[0].command.group.read().unwrap();
        let g1 = commands[1].command.group.read().unwrap();
        assert_eq!(g0.gid().value(), 200);
        assert_eq!(g1.gid().value(), 201);
    }

    #[test]
    fn test_create_multi_file_skips_files_without_urls() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<metalink xmlns="urn:ietf:params:xml:ns:metalink">
  <files>
    <file name="with-urls.bin">
      <size>1024</size>
      <url priority="1">http://mirror.example.com/with-urls.bin</url>
    </file>
    <file name="no-urls.bin">
      <size>2048</size>
    </file>
  </files>
</metalink>"#;

        let options = DownloadOptions::default();
        let commands =
            MetalinkDownloadCommand::create_multi_file(xml.as_bytes(), &options, None, 1).unwrap();

        assert_eq!(commands.len(), 1, "Should skip file with no URLs");
        assert_eq!(commands[0].file_index, 0);
    }

    #[test]
    fn test_output_path_accessor() {
        let options = DownloadOptions::default();
        let commands = MetalinkDownloadCommand::create_multi_file(
            &make_multi_file_xml(),
            &options,
            Some("/tmp"),
            1,
        )
        .unwrap();

        assert!(
            commands[0]
                .command
                .output_path()
                .to_string_lossy()
                .contains("first.bin")
        );
    }
}
