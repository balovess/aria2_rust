use async_trait::async_trait;
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::checksum::checksum::{Checksum, verify_file};
use crate::checksum::message_digest::HashType;
use crate::engine::active_output_registry::global_registry;
use crate::engine::command::{Command, CommandStatus};
use crate::engine::progress_checkpoint::ProgressCheckpoint;
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::GroupId;
#[cfg(feature = "bittorrent")]
use crate::request::request_group::MetadataInfo;
use crate::util::rwlock_ext::RwLockRecover;

use super::MetalinkDownloadCommand;

fn classify_metalink_http_status(status_code: u16) -> Aria2Error {
    if status_code == 404 {
        Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
    } else if status_code >= 500 {
        Aria2Error::Recoverable(RecoverableError::ServerError { code: status_code })
    } else {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: format!("HTTP error: {status_code}"),
        })
    }
}

struct PayloadDownload {
    path: PathBuf,
    completed_length: u64,
    total_length: u64,
}

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
    async fn complete_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.complete().await;
        }
    }

    async fn finalize_partial_writer(&mut self, writer: &mut Box<dyn DiskWriter>) {
        let _ = writer.finalize().await;
        self.flush_checkpoint().await;
    }

    async fn flush_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.as_mut() {
            let _ = self.group.recover().take_save_control_file_request();
            checkpoint.update(self.completed_bytes, true).await;
        }
    }

    async fn discard_checkpoint(&mut self, output_path: &Path) {
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.discard(output_path).await;
        } else if let Err(error) = tokio::fs::remove_file(output_path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                path = %output_path.display(),
                %error,
                "Failed to remove invalid Metalink output"
            );
        }
    }

    fn lifecycle_error(&self) -> Option<Aria2Error> {
        let group = self.group.recover();
        if group.is_removed() {
            Some(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            ))
        } else if group.is_paused_flag() {
            Some(Aria2Error::DownloadFailed("Download paused".into()))
        } else if group.is_force_halt_requested() || group.is_halt_requested() {
            Some(Aria2Error::DownloadFailed("Download halted".into()))
        } else {
            None
        }
    }

    async fn execute_file(
        &mut self,
        complete_group: bool,
        allow_torrent_fallback: bool,
    ) -> Result<()> {
        #[cfg(not(feature = "bittorrent"))]
        let _ = allow_torrent_fallback;

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

        let mut last_error = None;

        for url_entry in &sorted_urls_owned {
            debug!(
                "Trying mirror [priority={}] : {}",
                url_entry.priority, url_entry.url
            );

            match self
                .download_payload_url(&resolved_output_path, &url_entry.url, expected_size)
                .await
            {
                Ok(payload) => {
                    let hash_valid = match hash_entry_owned.as_ref() {
                        Some(hash) => match self.verify_file_hash(&payload.path, hash).await {
                            Ok(valid) => valid,
                            Err(error) => {
                                self.discard_checkpoint(&payload.path).await;
                                global_registry().release(&resolved_output_path).await;
                                return Err(error);
                            }
                        },
                        None => true,
                    };
                    if !hash_valid {
                        let hash = hash_entry_owned
                            .as_ref()
                            .expect("hash validation requires a hash entry");
                        warn!(
                            "Hash verification failed [{}]: trying next mirror",
                            hash.algo.as_standard_name()
                        );
                        self.discard_checkpoint(&payload.path).await;
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
                    let pieces_valid = match pieces_owned.as_ref() {
                        Some(pieces) => {
                            match self.verify_pieces_file(&payload.path, pieces).await {
                                Ok(valid) => valid,
                                Err(error) => {
                                    self.discard_checkpoint(&payload.path).await;
                                    global_registry().release(&resolved_output_path).await;
                                    return Err(error);
                                }
                            }
                        }
                        None => true,
                    };
                    if !pieces_valid {
                        warn!("Chunk hash verification failed: trying next mirror");
                        self.discard_checkpoint(&payload.path).await;
                        last_error = Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: "Chunk hash verification failed".to_string(),
                            },
                        ));
                        continue;
                    }

                    self.complete_checkpoint().await;
                    self.completed_bytes = payload.completed_length;

                    {
                        let g = self.group.recover();
                        if payload.total_length > 0 {
                            g.set_total_length(payload.total_length);
                        }
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
                    global_registry().release(&resolved_output_path).await;
                    return Ok(());
                }
                Err(e) => {
                    warn!("Mirror download failed {}: {}", url_entry.url, e);
                    if self.lifecycle_error().is_some() {
                        global_registry().release(&resolved_output_path).await;
                        return Err(e);
                    }
                    last_error = Some(e);
                }
            }
        }

        global_registry().release(&resolved_output_path).await;

        // All HTTP/FTP mirrors failed: fall back to the BitTorrent metaurl
        // dependency (mirrors C++ BtDependency resolving a torrent metaurl
        // when no direct resource can be downloaded).
        #[cfg(feature = "bittorrent")]
        if allow_torrent_fallback && !torrent_metaurls_owned.is_empty() {
            warn!("All HTTP mirrors failed, falling back to torrent metaurl");
            self.discard_checkpoint(&resolved_output_path).await;
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
                checkpoint: None,
                global_limiter: self.global_limiter.clone(),
                #[cfg(feature = "bittorrent")]
                public_tracker_catalog: self.public_tracker_catalog.clone(),
                #[cfg(feature = "bittorrent")]
                bt_registry: self.bt_registry.clone(),
                #[cfg(feature = "bittorrent")]
                bt_listener: self.bt_listener.clone(),
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

        if direct_failed && self.lifecycle_error().is_none() {
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
            match self.download_metadata_url(&mu.url).await {
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
                    #[cfg(feature = "bittorrent")]
                    if let Some(registry) = self.bt_registry.clone() {
                        bt_cmd.set_bt_registry(registry);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(listener) = self.bt_listener.clone() {
                        bt_cmd.set_bt_listener(listener);
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
                    .unwrap_or(crate::constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
                    .clamp(1, 16) as usize,
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
            match self.download_metadata_url(&metadata_uri).await {
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
                    #[cfg(feature = "bittorrent")]
                    if let Some(registry) = self.bt_registry.clone() {
                        bt_cmd.set_bt_registry(registry);
                    }
                    #[cfg(feature = "bittorrent")]
                    if let Some(listener) = self.bt_listener.clone() {
                        bt_cmd.set_bt_listener(listener);
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

    async fn download_payload_url(
        &mut self,
        output_path: &Path,
        url: &str,
        expected_size: Option<u64>,
    ) -> Result<PayloadDownload> {
        let existing_length = tokio::fs::metadata(output_path)
            .await
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let total_length = expected_size
            .or(ProgressCheckpoint::stored_total_length(output_path).await)
            .unwrap_or(0);
        let continue_download = self.group.recover().options().continue_download;
        let existing_length = if total_length > 0 && existing_length > total_length {
            truncate_output(output_path).await?;
            0
        } else {
            existing_length
        };
        let resume_input_length = if total_length > 0 {
            ProgressCheckpoint::resume_input_length(
                output_path,
                existing_length,
                continue_download,
                total_length,
            )
            .await
        } else if continue_download {
            existing_length
        } else {
            0
        };

        self.checkpoint = if total_length > 0 {
            Some(ProgressCheckpoint::open(output_path, total_length, resume_input_length).await)
        } else {
            None
        };
        let resume_offset = self
            .checkpoint
            .as_ref()
            .map(|checkpoint| checkpoint.resume_offset(resume_input_length))
            .unwrap_or(resume_input_length);

        if let Some(lifecycle_error) = self.lifecycle_error() {
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(resume_offset, true).await;
            }
            self.completed_bytes = resume_offset;
            return Err(lifecycle_error);
        }

        let mut request = self.client.get(url);
        if resume_offset > 0 {
            request = request.header(reqwest::header::RANGE, format!("bytes={resume_offset}-"));
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                if let Some(checkpoint) = self.checkpoint.as_mut() {
                    checkpoint.update(resume_offset, true).await;
                }
                self.completed_bytes = resume_offset;
                return Err(Aria2Error::Recoverable(
                    RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP request failed: {error}"),
                    },
                ));
            }
        };

        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(resume_offset, true).await;
            }
            return Err(classify_metalink_http_status(status.as_u16()));
        }

        if resume_offset > 0 && status.as_u16() == 200 {
            let always_resume = self.group.recover().options().always_resume;
            if !always_resume {
                truncate_output(output_path).await?;
                self.checkpoint = if total_length > 0 {
                    Some(ProgressCheckpoint::open(output_path, total_length, 0).await)
                } else {
                    None
                };
                self.completed_bytes = 0;
                return self
                    .download_payload_response(output_path, response, expected_size, 0)
                    .await;
            }
            if let Some(checkpoint) = self.checkpoint.as_mut() {
                checkpoint.update(resume_offset, true).await;
            }
            self.completed_bytes = resume_offset;
            return Err(Aria2Error::Recoverable(RecoverableError::CannotResume));
        }

        self.download_payload_response(output_path, response, expected_size, resume_offset)
            .await
    }

    async fn download_payload_response(
        &mut self,
        output_path: &Path,
        response: reqwest::Response,
        expected_size: Option<u64>,
        resume_offset: u64,
    ) -> Result<PayloadDownload> {
        // Content-Range is authoritative for a resumed response. For a fresh
        // response, use the body length and the Metalink size when present.
        let response_length = response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        let advertised_total = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range_total)
            .or_else(|| (response_length > 0).then_some(resume_offset + response_length))
            .or(expected_size)
            .unwrap_or(0);
        let total_length = expected_size.unwrap_or(advertised_total);

        if self.checkpoint.is_none() && total_length > 0 {
            self.checkpoint = Some(
                ProgressCheckpoint::open(
                    output_path,
                    total_length,
                    resume_offset.min(total_length),
                )
                .await,
            );
        }

        {
            let g = self.group.recover();
            g.set_total_length(total_length);
            g.update_progress(resume_offset);
        }

        let raw_writer = if resume_offset > 0 {
            DefaultDiskWriter::new_with_offset(output_path, resume_offset)
        } else {
            DefaultDiskWriter::new(output_path)
        };
        let rate_limit = self.group.recover().options().max_download_limit;
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|limiter| limiter.is_download_limited());
        let mut writer: Box<dyn DiskWriter> = if rate_limit.is_some() || global_limited {
            let per_rate = rate_limit.filter(|&rate| rate > 0);
            let limiter = per_rate
                .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)))
                .unwrap_or_else(RateLimiter::unlimited);
            let mut writer = ThrottledWriter::new(raw_writer, limiter);
            if let Some(global_limiter) = self.global_limiter.as_ref() {
                writer = writer.with_global_limiter(global_limiter.clone());
            }
            Box::new(writer)
        } else {
            Box::new(raw_writer)
        };

        self.completed_bytes = resume_offset;
        let mut stream = response.bytes_stream();
        let mut last_speed_update = Instant::now();
        let mut last_completed = 0u64;

        while let Some(chunk_result) = tokio::select! {
            next = stream.next() => next,
            _ = self.wait_for_lifecycle_change() => {
                self.finalize_partial_writer(&mut writer).await;
                return Err(self.lifecycle_error().unwrap_or_else(|| {
                    Aria2Error::DownloadFailed("Metalink download halted".into())
                }));
            }
        } {
            let bytes: bytes::Bytes = match chunk_result {
                Ok(bytes) => bytes,
                Err(error) => {
                    self.finalize_partial_writer(&mut writer).await;
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::TemporaryNetworkFailure {
                            message: error.to_string(),
                        },
                    ));
                }
            };
            if let Some(lifecycle_error) = self.lifecycle_error() {
                writer.finalize().await.ok();
                self.flush_checkpoint().await;
                return Err(lifecycle_error);
            }
            if let Err(error) = writer.write(&bytes).await {
                self.finalize_partial_writer(&mut writer).await;
                return Err(Aria2Error::FileIo(format!(
                    "Failed to write Metalink payload: {error}"
                )));
            }
            self.completed_bytes = self.completed_bytes.saturating_add(bytes.len() as u64);

            if let Some(checkpoint) = self.checkpoint.as_mut() {
                let save_requested = self.group.recover().take_save_control_file_request();
                checkpoint
                    .update(self.completed_bytes, save_requested)
                    .await;
            }

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

        writer.finalize().await.map_err(|error| {
            Aria2Error::FileIo(format!("Failed to finalize Metalink file: {error}"))
        })?;

        if let Some(expected) = expected_size
            && self.completed_bytes != expected
        {
            self.discard_checkpoint(output_path).await;
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: format!(
                        "Metalink size mismatch: expected {} bytes, received {}",
                        expected, self.completed_bytes
                    ),
                },
            ));
        }

        Ok(PayloadDownload {
            path: output_path.to_path_buf(),
            completed_length: self.completed_bytes,
            total_length: total_length.max(self.completed_bytes),
        })
    }

    async fn wait_for_lifecycle_change(&self) {
        loop {
            if self.lifecycle_error().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[cfg(feature = "bittorrent")]
    async fn download_metadata_url(&self, url: &str) -> Result<Vec<u8>> {
        let response = self.client.get(url).send().await.map_err(|e| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: format!("HTTP request failed: {e}"),
            })
        })?;
        let status = response.status();
        if !status.is_success() && status.as_u16() != 206 {
            return Err(classify_metalink_http_status(status.as_u16()));
        }
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("HTTP metadata read failed: {e}"),
                })
            })
    }

    async fn verify_file_hash(
        &self,
        path: &Path,
        hash: &aria2_protocol::metalink::parser::HashEntry,
    ) -> Result<bool> {
        let hash_type = HashType::from_str(hash.algo.as_standard_name())
            .ok_or_else(|| Aria2Error::Parse("unsupported Metalink hash algorithm".into()))?;
        let checksum = Checksum::new(hash_type, &hash.value)?;
        verify_file(path, &checksum).await
    }

    /// Verify a whole-file download against Metalink `<pieces>` chunk hashes.
    ///
    /// Mirrors C++ `MetalinkEntry::checksum` / `ChunkChecksum` verification:
    /// the data is split into `pieces.length`-sized chunks and each chunk is
    /// compared against its corresponding digest. Returns `Ok(false)` on any
    /// mismatch, when the number of digests does not match the expected piece
    /// count, or when a digest has the wrong hex length.
    #[cfg(test)]
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
            if !actual.eq_ignore_ascii_case(expected_hash) {
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

    async fn verify_pieces_file(
        &self,
        path: &Path,
        pieces: &aria2_protocol::metalink::parser::PieceInfo,
    ) -> Result<bool> {
        if pieces.hashes.is_empty() {
            return Ok(true);
        }
        if pieces.length == 0 {
            return Ok(false);
        }
        let metadata = tokio::fs::metadata(path)
            .await
            .map_err(|error| Aria2Error::FileIo(error.to_string()))?;
        let expected = pieces.num_pieces(metadata.len());
        if pieces.hashes.len() != expected
            || pieces
                .hashes
                .iter()
                .any(|hash| hash.len() != pieces.type_.hash_len())
        {
            return Ok(false);
        }

        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| Aria2Error::FileIo(error.to_string()))?;
        let mut buffer = vec![0u8; pieces.length as usize];
        for (index, expected_hash) in pieces.hashes.iter().enumerate() {
            let mut read = 0usize;
            while read < buffer.len() {
                let count = tokio::io::AsyncReadExt::read(&mut file, &mut buffer[read..]).await?;
                if count == 0 {
                    break;
                }
                read += count;
            }
            let actual = digest_hex(&buffer[..read], pieces.type_);
            if !actual.eq_ignore_ascii_case(expected_hash) {
                warn!(piece = index, "Metalink piece hash mismatch");
                return Ok(false);
            }
            if read < buffer.len() && index + 1 < pieces.hashes.len() {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

async fn truncate_output(path: &Path) -> Result<()> {
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await
        .map_err(|error| Aria2Error::FileIo(error.to_string()))?;
    file.sync_data()
        .await
        .map_err(|error| Aria2Error::FileIo(error.to_string()))
}

fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.parse().ok()
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
        HashAlgorithm::Sha224 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha224::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha256 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha256::new();
            hasher.update(data);
            format!("{:x}", hasher.finalize())
        }
        HashAlgorithm::Sha384 => {
            use sha2::Digest;
            let mut hasher = sha2::Sha384::new();
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

#[cfg(test)]
mod http_status_tests {
    use super::*;

    #[test]
    fn classifies_5xx_as_retryable_server_errors() {
        assert!(matches!(
            classify_metalink_http_status(503),
            Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 })
        ));
    }

    #[test]
    fn classifies_not_found_as_resource_not_found() {
        assert!(matches!(
            classify_metalink_http_status(404),
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
        ));
    }
}
