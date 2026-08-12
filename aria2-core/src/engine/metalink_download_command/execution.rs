use async_trait::async_trait;
use futures::StreamExt;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::active_output_registry::global_registry;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::GroupId;
#[cfg(feature = "bittorrent")]
use crate::request::request_group::MetadataInfo;
use crate::util::rwlock_ext::RwLockRecover;

use super::MetalinkDownloadCommand;

#[async_trait]
impl Command for MetalinkDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        if !self.grouped_file_infos.is_empty() {
            return self.execute_grouped().await;
        }

        self.execute_file(true, true).await
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

    fn request_group(
        &self,
    ) -> Option<std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>>
    {
        Some(std::sync::Arc::clone(&self.group))
    }

    fn timeout(&self) -> Option<Duration> {
        Some(Duration::from_secs(600))
    }
}

impl MetalinkDownloadCommand {
    async fn execute_file(
        &mut self,
        complete_group: bool,
        allow_torrent_fallback: bool,
    ) -> Result<()> {
        // Resolve file info: either from pre-parsed file_info (multi-file mode)
        // or by re-parsing the raw metalink_data (single-file mode).
        // We extract owned data to avoid lifetime/borrow issues.
        let sorted_urls_owned: Vec<aria2_protocol::metalink::parser::UrlEntry>;
        let expected_size: Option<u64>;
        let hash_entry_owned: Option<aria2_protocol::metalink::parser::HashEntry>;
        let pieces_owned: Option<aria2_protocol::metalink::parser::PieceInfo>;
        let torrent_metaurls_owned: Vec<aria2_protocol::metalink::parser::MetaUrlEntry>;

        match &self.file_info {
            Some(info) => {
                sorted_urls_owned = info.sorted_urls.clone();
                expected_size = info.expected_size;
                hash_entry_owned = info.hash_entry.clone();
                pieces_owned = info.pieces.clone();
                torrent_metaurls_owned = info.torrent_metaurls.clone();
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
                hash_entry_owned = file.strongest_hash().cloned();
                pieces_owned = file.pieces.clone();
                torrent_metaurls_owned = file
                    .meta_urls
                    .iter()
                    .filter(|m| m.mediatype == aria2_protocol::metalink::parser::MediaType::Torrent)
                    .cloned()
                    .collect();

                if sorted_urls_owned.is_empty() && torrent_metaurls_owned.is_empty() {
                    return Err(Aria2Error::Fatal(FatalError::Config(
                        "No download mirrors available".into(),
                    )));
                }
            }
        }

        if sorted_urls_owned.is_empty() {
            // No HTTP/FTP mirrors, but a torrent metaurl is present: fall
            // straight through to the BitTorrent dependency path.
            if torrent_metaurls_owned.is_empty() {
                return Err(Aria2Error::Fatal(FatalError::Config(
                    "No download mirrors available".into(),
                )));
            }
            #[cfg(feature = "bittorrent")]
            if allow_torrent_fallback {
                return self.try_torrent_metaurl(&torrent_metaurls_owned).await;
            }
            #[cfg(not(feature = "bittorrent"))]
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

                    // Chunk-level verification (<pieces>) — mirrors C++
                    // `MetalinkEntry::chunkChecksum` checking after download.
                    if let Some(ref pieces) = pieces_owned
                        && !self.verify_pieces(&data, pieces)?
                    {
                        warn!("Chunk hash verification failed: trying next mirror");
                        last_error = Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: "Chunk hash verification failed".to_string(),
                            },
                        ));
                        continue;
                    }

                    let raw_writer = DefaultDiskWriter::new(&resolved_output_path);
                    let rate_limit = {
                        let g = self.group.recover();
                        g.options().max_download_limit
                    };
                    // Global (process-wide) limiter: when present and enabled,
                    // the writer acquires tokens after the per-download limiter.
                    let global_limited = self
                        .global_limiter
                        .as_ref()
                        .is_some_and(|g| g.is_download_limited());
                    let mut writer: Box<dyn DiskWriter> = if rate_limit.is_some() || global_limited
                    {
                        let per_rate = rate_limit.filter(|&r| r > 0);
                        let limiter = per_rate
                            .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)))
                            .unwrap_or_else(RateLimiter::unlimited);
                        let mut tw = ThrottledWriter::new(raw_writer, limiter);
                        if let Some(ref gl) = self.global_limiter {
                            tw = tw.with_global_limiter(gl.clone());
                        }
                        Box::new(tw)
                    } else {
                        Box::new(raw_writer)
                    };
                    writer.write(&data).await?;
                    writer.finalize().await.map_err(|error| {
                        Aria2Error::FileIo(format!("Failed to finalize Metalink file: {error}"))
                    })?;

                    self.completed_bytes = data.len() as u64;

                    {
                        let g = self.group.recover();
                        g.update_progress(self.completed_bytes);
                        g.update_speed(self.completed_bytes, 0);
                        drop(g);
                        if complete_group {
                            let mut g = self.group.recover_mut();
                            g.complete()?;
                        }
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

        // All HTTP/FTP mirrors failed: fall back to the BitTorrent metaurl
        // dependency (mirrors C++ BtDependency resolving a torrent metaurl
        // when no direct resource can be downloaded).
        #[cfg(feature = "bittorrent")]
        if allow_torrent_fallback && !torrent_metaurls_owned.is_empty() {
            warn!("All HTTP mirrors failed, falling back to torrent metaurl");
            return self.try_torrent_metaurl(&torrent_metaurls_owned).await;
        }

        Err(last_error
            .unwrap_or_else(|| Aria2Error::Fatal(FatalError::Config("All mirrors failed".into()))))
    }
}

impl MetalinkDownloadCommand {
    async fn execute_grouped(&mut self) -> Result<()> {
        let grouped_files = std::mem::take(&mut self.grouped_file_infos);
        let mut completed_bytes = 0u64;
        let mut direct_failed = false;

        for (path, info) in &grouped_files {
            let mut command = Self {
                group: std::sync::Arc::clone(&self.group),
                client: self.client.clone(),
                output_path: path.clone(),
                started: true,
                completed: false,
                completed_bytes: 0,
                metalink_data: Vec::new(),
                file_info: Some(info.clone()),
                grouped_file_infos: Vec::new(),
                global_limiter: self.global_limiter.clone(),
                #[cfg(feature = "bittorrent")]
                public_tracker_catalog: self.public_tracker_catalog.clone(),
            };
            match command.execute_file(false, false).await {
                Ok(()) => {
                    completed_bytes = completed_bytes.saturating_add(command.completed_bytes);
                }
                Err(error) => {
                    direct_failed = true;
                    warn!(
                        path = %command.output_path.display(),
                        error = %error,
                        "Shared Metalink direct mirror failed"
                    );
                }
            }
        }

        if direct_failed {
            #[cfg(feature = "bittorrent")]
            {
                warn!("At least one shared Metalink mirror failed, using one torrent fallback");
                return self.try_torrent_metaurl_group(&grouped_files).await;
            }
            #[cfg(not(feature = "bittorrent"))]
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Shared Metalink mirrors failed and BitTorrent support is disabled".into(),
            )));
        }

        self.completed_bytes = completed_bytes;
        self.completed = true;
        let mut group = self.group.recover_mut();
        group.update_progress(completed_bytes);
        group.set_completed_length(completed_bytes);
        group.complete()?;
        Ok(())
    }

    /// Download a `.torrent` from the given metaurls (by priority) and run a
    /// BitTorrent download for it. Mirrors C++ `BtDependency` which resolves
    /// `metaurl mediatype="application/x-bittorrent"` entries.
    #[cfg(feature = "bittorrent")]
    pub(crate) async fn try_torrent_metaurl(
        &mut self,
        meta_urls: &[aria2_protocol::metalink::parser::MetaUrlEntry],
    ) -> Result<()> {
        use aria2_protocol::metalink::parser::MediaType;

        let mut last_err: Option<Aria2Error> = None;
        for mu in meta_urls
            .iter()
            .filter(|m| m.mediatype == MediaType::Torrent)
        {
            info!(url = %mu.url, "Downloading torrent from Metalink metaurl");
            match self.try_download_url(&mu.url, None).await {
                Ok(torrent_bytes) => {
                    // Persist metadata beside the payload so the dependency
                    // can be reconstructed by the manager and after restart.
                    let metadata_path = self.output_path.with_extension("torrent");
                    tokio::fs::write(&metadata_path, &torrent_bytes)
                        .await
                        .map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to persist torrent metadata '{}': {error}",
                                metadata_path.display()
                            ))
                        })?;

                    let options = self.group.recover().options().clone();
                    let gid = self.group.recover().gid();
                    let dir = self.output_path.parent().and_then(|p| p.to_str());
                    {
                        let group = self.group.recover_mut();
                        group.set_metadata_info(
                            MetadataInfo::new(gid, &mu.url)
                                .with_metadata_path(metadata_path.to_string_lossy()),
                        );
                    }
                    let mut bt_cmd =
                        crate::engine::bt_download_command::BtDownloadCommand::new_with_group(
                            std::sync::Arc::clone(&self.group),
                            &torrent_bytes,
                            &options,
                            dir,
                        )?;
                    if let Some(gl) = self.global_limiter.clone() {
                        bt_cmd.set_global_limiter(gl);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(catalog) = self.public_tracker_catalog.clone() {
                        bt_cmd.set_public_tracker_catalog(catalog);
                    }
                    bt_cmd.execute().await?;
                    self.completed_bytes = self.group.recover().total_length();
                    {
                        let mut group = self.group.recover_mut();
                        group.update_progress(self.completed_bytes);
                        group.complete()?;
                    }
                    self.completed = true;
                    info!(
                        "Metalink torrent metaurl download done: {}",
                        self.output_path.display()
                    );
                    return Ok(());
                }
                Err(e) => {
                    warn!(url = %mu.url, error = %e, "Torrent metaurl failed");
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Fatal(FatalError::Config("All torrent metaurls failed".into()))
        }))
    }

    /// Resolve one torrent metaurl for every file in a shared Metalink group.
    ///
    /// The torrent is parsed once and the selected Metalink paths/mirrors are
    /// applied to one BitTorrent context. This preserves multi-file offsets
    /// and avoids the per-file metadata loss caused by independent fallback.
    #[cfg(feature = "bittorrent")]
    async fn try_torrent_metaurl_group(
        &mut self,
        grouped_files: &[(std::path::PathBuf, super::types::FileDownloadInfo)],
    ) -> Result<()> {
        use crate::request::request_group::BtFileMapping;
        use aria2_protocol::metalink::parser::MediaType;

        let mut meta_urls = Vec::new();
        for (_, info) in grouped_files {
            for metaurl in &info.torrent_metaurls {
                if metaurl.mediatype == MediaType::Torrent
                    && !meta_urls
                        .iter()
                        .any(|candidate: &String| candidate == &metaurl.url)
                {
                    meta_urls.push(metaurl.url.clone());
                }
            }
        }
        if meta_urls.is_empty() {
            return Err(Aria2Error::Fatal(FatalError::Config(
                "Shared Metalink group has no torrent metaurl".into(),
            )));
        }

        let mappings = grouped_files
            .iter()
            .map(|(path, info)| BtFileMapping {
                original_name: info
                    .torrent_metaurls
                    .iter()
                    .find(|metaurl| metaurl.mediatype == MediaType::Torrent)
                    .and_then(|metaurl| metaurl.name.clone())
                    .unwrap_or_default(),
                path: path.to_string_lossy().into_owned(),
                uris: info
                    .sorted_urls
                    .iter()
                    .filter(|url| url.is_non_p2p())
                    .map(|url| url.url.clone())
                    .collect(),
                max_connection_per_server: self
                    .group
                    .recover()
                    .options()
                    .max_connection_per_server
                    .unwrap_or(1)
                    .max(1) as usize,
                unique_protocol: self
                    .group
                    .recover()
                    .options()
                    .metalink_enable_unique_protocol,
            })
            .collect::<Vec<_>>();

        let mut last_err = None;
        for metadata_uri in meta_urls {
            info!(url = %metadata_uri, "Downloading shared torrent from Metalink metaurl");
            match self.try_download_url(&metadata_uri, None).await {
                Ok(torrent_bytes) => {
                    let metadata_path = self.output_path.with_extension("torrent");
                    tokio::fs::write(&metadata_path, &torrent_bytes)
                        .await
                        .map_err(|error| {
                            Aria2Error::FileIo(format!(
                                "Failed to persist torrent metadata '{}': {error}",
                                metadata_path.display()
                            ))
                        })?;

                    let options = self.group.recover().options().clone();
                    let gid = self.group.recover().gid();
                    let dir = self.output_path.parent().and_then(|path| path.to_str());
                    self.group.recover_mut().set_metadata_info(
                        MetadataInfo::new(gid, &metadata_uri)
                            .with_metadata_path(metadata_path.to_string_lossy()),
                    );

                    let mut bt_cmd = crate::engine::bt_download_command::BtDownloadCommand::new_with_group_and_mappings(
                        std::sync::Arc::clone(&self.group),
                        &torrent_bytes,
                        &options,
                        dir,
                        &mappings,
                    )?;
                    if let Some(global_limiter) = self.global_limiter.clone() {
                        bt_cmd.set_global_limiter(global_limiter);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(catalog) = self.public_tracker_catalog.clone() {
                        bt_cmd.set_public_tracker_catalog(catalog);
                    }
                    bt_cmd.execute().await?;
                    self.completed_bytes = self.group.recover().completed_length();
                    self.completed = true;
                    info!(
                        path = %self.output_path.display(),
                        bytes = self.completed_bytes,
                        "Shared Metalink torrent fallback completed"
                    );
                    return Ok(());
                }
                Err(error) => {
                    warn!(url = %metadata_uri, error = %error, "Shared torrent metaurl failed");
                    last_err = Some(error);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Fatal(FatalError::Config(
                "All shared torrent metaurls failed".into(),
            ))
        }))
    }

    pub(crate) async fn try_download_url(
        &mut self,
        url: &str,
        expected_size: Option<u64>,
    ) -> Result<Vec<u8>> {
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

        if let Some(expected) = expected_size
            && data.len() as u64 != expected
        {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Metalink size mismatch: expected {} bytes, received {}",
                        expected,
                        data.len()
                    ),
                },
            ));
        }

        Ok(data)
    }

    pub(crate) fn verify_hash(
        &self,
        data: &[u8],
        hash: &aria2_protocol::metalink::parser::HashEntry,
    ) -> Result<bool> {
        let digest = digest_hex(data, hash.algo);
        Ok(digest.eq_ignore_ascii_case(&hash.value))
    }

    /// Verify a whole-file download against Metalink `<pieces>` chunk hashes.
    ///
    /// Mirrors C++ `MetalinkEntry::checksum` / `ChunkChecksum` verification:
    /// the data is split into `pieces.length`-sized chunks and each chunk is
    /// compared against its corresponding digest. Returns `Ok(false)` on any
    /// mismatch, when the number of digests does not match the expected piece
    /// count, or when a digest has the wrong hex length.
    pub(crate) fn verify_pieces(
        &self,
        data: &[u8],
        pieces: &aria2_protocol::metalink::parser::PieceInfo,
    ) -> Result<bool> {
        if pieces.hashes.is_empty() {
            return Ok(true);
        }
        let hex_len = pieces.type_.hash_len();
        if pieces.hashes.iter().any(|h| h.len() != hex_len) {
            warn!(
                algo = ?pieces.type_,
                "Metalink pieces digest length mismatch, verification failed"
            );
            return Ok(false);
        }

        let expected = pieces.num_pieces(data.len() as u64);
        if pieces.hashes.len() != expected {
            warn!(
                expected,
                actual = pieces.hashes.len(),
                "Metalink pieces count mismatch, verification failed"
            );
            return Ok(false);
        }

        let chunk_len = pieces.length as usize;
        for (i, expected_hash) in pieces.hashes.iter().enumerate() {
            let start = i * chunk_len;
            let end = ((i + 1) * chunk_len).min(data.len());
            let chunk = &data[start..end];
            let actual = digest_hex(chunk, pieces.type_);
            if actual != *expected_hash {
                warn!(
                    piece = i,
                    "Metalink piece hash mismatch ({} / {})",
                    i + 1,
                    pieces.hashes.len()
                );
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Compute the lowercase hex digest of `data` for a Metalink hash algorithm.
fn digest_hex(data: &[u8], algo: aria2_protocol::metalink::parser::HashAlgorithm) -> String {
    use aria2_protocol::metalink::parser::HashAlgorithm;
    match algo {
        HashAlgorithm::Md5 => {
            use md5::Digest;
            let mut hasher = md5::Md5::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha1 => {
            use sha1::Digest;
            let mut hasher = sha1::Sha1::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha256 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha512 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha512::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
    }
}
