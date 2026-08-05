mod execution;
#[cfg(test)]
mod tests;
mod types;

pub use types::{select_mirrors_by_priority, try_mirrors_with_failover};

use std::sync::Arc;
use std::time::Duration;

use aria2_protocol::metalink::parser::UrlEntry;
use tracing::info;

use crate::error::{Aria2Error, FatalError, Result};
use crate::rate_limiter::RateLimiter;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

use types::FileDownloadInfo;

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
    pub(crate) group: Arc<std::sync::RwLock<RequestGroup>>,
    pub(crate) client: reqwest::Client,
    pub(crate) output_path: std::path::PathBuf,
    pub(crate) started: bool,
    pub(crate) completed: bool,
    pub(crate) completed_bytes: u64,
    /// Raw Metalink data for re-parsing during execute().
    /// Only used for single-file mode. Empty in multi-file mode
    /// (each per-file command stores only its own file's data).
    pub(crate) metalink_data: Vec<u8>,
    /// Parsed file info for per-file mode (set by create_multi_file).
    /// When present, execute() uses this instead of re-parsing metalink_data.
    pub(crate) file_info: Option<FileDownloadInfo>,
    /// Process-wide rate limiter from `DownloadEngine::global_limiter`.
    /// When `Some`, passed down to `ThrottledWriter` for mirror downloads.
    pub(crate) global_limiter: Option<RateLimiter>,
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
            // A torrent metaurl is still a valid download path (C++
            // BtDependency); reject only when there is nothing at all.
            let has_torrent_metaurl = file
                .meta_urls
                .iter()
                .any(|m| m.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent);
            if !has_torrent_metaurl {
                return Err(Aria2Error::Fatal(FatalError::Config(
                    "Metalink file has no download URL".into(),
                )));
            }
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
            global_limiter: None,
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
                tracing::debug!(
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
                pieces: file.pieces.clone(),
                torrent_metaurls: file
                    .meta_urls
                    .iter()
                    .filter(|m| m.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent)
                    .cloned()
                    .collect(),
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
                    global_limiter: None,
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

        let torrent_metaurls: Vec<_> = file
            .meta_urls
            .iter()
            .filter(|metaurl| {
                metaurl.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent
            })
            .cloned()
            .collect();
        if file.urls.is_empty() && torrent_metaurls.is_empty() {
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

        if urls.is_empty() && torrent_metaurls.is_empty() {
            tracing::debug!(
                name = %file.name,
                "Skipping Metalink file with no downloadable resources"
            );
            return Ok(Vec::new());
        }

        let group = RequestGroup::new(gid, urls, options.clone());

        let file_info = FileDownloadInfo {
            expected_size: file.size,
            hash_entry: file.strongest_hash().cloned(),
            sorted_urls,
            pieces: file.pieces.clone(),
            torrent_metaurls,
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
                global_limiter: None,
            },
            file_index: 0,
        }])
    }

    /// Get the output path for this download.
    pub fn output_path(&self) -> &std::path::Path {
        &self.output_path
    }

    /// Set the process-wide rate limiter (from `DownloadEngine::global_limiter`).
    ///
    /// When set, mirror downloads acquire tokens from this limiter in addition
    /// to the per-download limiter.
    pub fn set_global_limiter(&mut self, limiter: RateLimiter) {
        self.global_limiter = Some(limiter);
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
pub(crate) fn build_http_client() -> Result<reqwest::Client> {
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
