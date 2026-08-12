use async_trait::async_trait;
use futures::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::checksum::checksum::{Checksum, verify_file};
use crate::checksum::message_digest::HashType;
use crate::constants;
use crate::engine::active_output_registry::{OutputPathPolicy, global_registry};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::concurrent_download::{ConcurrentDownloadResult, ConcurrentDownloader};
use crate::engine::download_cookie::CookieHelper;
use crate::engine::range_prober::RangeProber;
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::sequential_download::SequentialDownloader;
use crate::error::{Aria2Error, Result};
use crate::filesystem::file_allocation;
use crate::filesystem::file_allocation_man;
use crate::filesystem::resume_helper::ResumeHelper;
use crate::request::request_group::{DownloadResultCode, GroupId};
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    async fn execute_attempt(&mut self, uri: &str) -> Result<()> {
        debug!(
            "Starting download: {} -> {}",
            uri,
            self.output_path.display()
        );

        // Re-check cancellation after the HEAD probe / pre-allocation work so
        // a remove issued while we were doing filesystem setup is honoured
        // before any network transfer begins.
        self.check_cancelled()?;

        if let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Failed to create directory: {}",
                    e
                )))
            })?;
        }

        let release_path = |path: &std::path::Path| {
            let path = path.to_path_buf();
            async move {
                global_registry().release(&path).await;
            }
        };

        let url_for_head = reqwest::Url::parse(uri).ok();
        let cookie_hdr_head = if let Some(ref url) = url_for_head {
            CookieHelper::new(Arc::clone(&self.cookie_storage), self.cookie_file.clone())
                .build_cookie_header_from_url(url)
        } else {
            String::new()
        };
        let options = self.group.recover().options_arc();
        let known_total_length = self.group.recover().total_length();
        let should_head = options.dry_run || (options.use_head && known_total_length == 0);
        let head_resp = if should_head {
            let head_req = self.request_policy.apply(
                self.client.head(uri),
                (!cookie_hdr_head.is_empty()).then_some(cookie_hdr_head.as_str()),
                &[],
            );
            head_req.send().await.ok()
        } else {
            None
        };
        let (total_length, head_supports_range) = if let Some(ref resp) = head_resp {
            let tl = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
            let sr = resp
                .headers()
                .get("Accept-Ranges")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.to_lowercase().contains("bytes"));
            (tl, sr)
        } else {
            (known_total_length, false)
        };

        let supports_range = if head_supports_range {
            true
        } else if total_length > constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            let prober = RangeProber::new(Arc::clone(&self.client), self.request_policy.clone())
                .with_cookie_header(
                    url_for_head
                        .as_ref()
                        .map(|url| {
                            self.create_cookie_helper()
                                .build_cookie_header_from_url(url)
                        })
                        .filter(|header| !header.is_empty()),
                );
            prober.probe_range_support(uri, total_length).await
        } else {
            false
        };

        let original_path = self.output_path.clone();
        if options.remove_control_file {
            let control_path =
                crate::filesystem::control_file::ControlFile::control_path_for(&original_path);
            match tokio::fs::remove_file(&control_path).await {
                Ok(()) => info!(path = %control_path.display(), "Removed requested control file"),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Aria2Error::FileIo(format!(
                        "Failed to remove control file {}: {}",
                        control_path.display(),
                        error
                    )));
                }
            }
        }
        self.output_path = global_registry()
            .resolve_with_policy(
                &original_path,
                OutputPathPolicy {
                    allow_overwrite: options.allow_overwrite,
                    auto_file_renaming: options.auto_file_renaming,
                    continue_download: options.continue_download,
                    check_integrity: self.check_integrity,
                    total_length: (total_length > 0).then_some(total_length),
                },
            )
            .await?;
        if self.output_path != original_path {
            info!(
                "Filename collision resolved: '{}' -> '{}'",
                original_path.display(),
                self.output_path.display()
            );
        }

        let continue_download = options.continue_download;
        let resume_helper = ResumeHelper::new(&self.output_path, continue_download);
        let mut resume_state = match resume_helper.detect(total_length).await {
            Ok(state) => state,
            Err(error) => {
                release_path(&self.output_path).await;
                return Err(error);
            }
        };

        // When resuming from a paused state, never short-circuit as "complete" --
        // the file on disk may be a preallocated sparse file that matches
        // total_length but hasn't actually been fully written. A download that
        // was explicitly paused by the user must always continue from where it
        // left off, relying on the control file's bitfield to determine which
        // ranges still need fetching.
        let was_paused = self.group.recover().is_paused_flag();

        if resume_state.is_complete && !was_paused && !self.check_integrity {
            info!(
                "File already exists completely, skipping download: {} ({} bytes)",
                self.output_path.display(),
                resume_state.existing_length
            );
            self.completed_bytes = resume_state.existing_length;
            {
                let g = self.group.recover();
                g.set_total_length(self.completed_bytes);
                g.update_progress(self.completed_bytes);
                g.set_completed_length(self.completed_bytes);
            }
            {
                let mut g = self.group.recover_mut();
                g.complete()?;
            }
            self.completed = true;
            release_path(&self.output_path).await;
            return Ok(());
        }

        // Final cancellation check before kicking off the (potentially long)
        // network transfer. If the task was removed during the HEAD probe or
        // resume detection, abort now rather than downloading data that will
        // just be discarded. This is placed before spawn_progress_aggregator
        // so a cancelled task does not spawn an unnecessary aggregator task.
        // Release the registered output path so future downloads can reuse
        // the filename.
        if let Err(e) = self.check_cancelled() {
            release_path(&self.output_path).await;
            return Err(e);
        }

        // Initialize tail reclaim progress tracking before the download loop.
        // Mirrors C++ DownloadCommand constructor which initializes
        // lastTailReclaimSessionDownloadLength_ to 0.
        self.update_tail_reclaim_progress();

        let download_result: Result<()> = async {
            // --check-integrity: when the download context carries piece
            // hashes (e.g. Metalink), verify the existing file chunk-by-chunk
            // before allocating/downloading (mirrors C++ CheckIntegrityMan +
            // CheckIntegrityCommand). No-op when there is nothing to validate.
            if self.check_integrity && total_length > 0 {
                use crate::checksum::check_integrity::man as ci_man;
                ci_man::cut_trailing_garbage(&self.output_path, total_length).await?;
                use crate::checksum::message_digest::HashType;
                // Extract owned data first so the RwLock guard is dropped
                // before any await (guard is not Send).
                let piece_info = self
                    .group
                    .recover()
                    .get_download_context()
                    .map(|ctx| {
                        (
                            ctx.get_piece_hashes().to_vec(),
                            ctx.get_piece_length() as u64,
                            ctx.get_piece_hash_type().to_string(),
                        )
                    });
                if let Some((hashes, piece_len, hash_type)) = piece_info {
                    let algo =
                        HashType::from_str(&hash_type).unwrap_or(HashType::Sha1);
                    if let Some(task) = ci_man::file_task(
                        &self.output_path,
                        piece_len.max(1),
                        total_length,
                        hashes,
                        algo,
                    )? {
                        let gid = self.group.recover().gid().value();
                        info!(gid, "Checking integrity of existing data against piece hashes");
                        let ok = ci_man::enqueue(&ci_man::shared(), gid, task).await?;
                        if !ok {
                            warn!(
                                gid,
                                "Integrity check failed; discarding resume state and re-downloading"
                            );
                            // C++ StreamCheckIntegrityEntry::onDownloadIncomplete()
                            // sends the request back through allocation/download rather
                            // than terminating the request. Do not reuse offsets derived
                            // from data that failed validation.
                            resume_state.should_resume = false;
                            resume_state.start_offset = 0;
                            resume_state.is_complete = false;
                        } else {
                            info!(gid, "Integrity check passed");
                            // A successful pre-download integrity check proves
                            // the complete existing file is already usable. Do
                            // not issue a range request at EOF or reallocate it.
                            if resume_state.existing_length >= total_length {
                                self.completed_bytes = total_length;
                                resume_state.start_offset = total_length;
                                resume_state.is_complete = true;
                                {
                                    let mut group = self.group.recover_mut();
                                    group.set_completed_length(total_length);
                                    group.complete()?;
                                }
                                self.completed = true;
                                release_path(&self.output_path).await;
                                return Ok(());
                            }
                        }
                    }
                }
            }

            if total_length > 0 {
                // Queue the allocation through the shared FileAllocationMan
                // (mirrors C++ FileAllocationMan + FileAllocationCommand):
                // the background worker drives chunked allocation sequentially
                // across downloads and yields between chunks, so a huge
                // zero-fill never blocks a worker thread or starves other
                // downloads. This task resumes once the file is ready.
                let strategy = file_allocation::AllocationStrategy::from_str(&self.file_allocation);
                if strategy != file_allocation::AllocationStrategy::None {
                    let gid = self.group.recover().gid().value();
                    file_allocation_man::enqueue_path(
                        &file_allocation_man::shared(),
                        &self.output_path,
                        total_length,
                        strategy,
                        self.secure_falloc,
                        gid,
                    )
                    .await?;
                }
            }

            let options = self.group.recover().options_arc();
            let split = options.split.unwrap_or(constants::DEFAULT_SPLIT);

            let cookie_helper = self.create_cookie_helper();
            let progress_updater = self.create_progress_updater();

            if self.should_use_concurrent(total_length, supports_range, split)
                && !options.http_accept_gzip
            {
                if resume_state.should_resume {
                    info!(
                        "Concurrent mode + resume: existing {} bytes, continuing from offset {}",
                        resume_state.existing_length, resume_state.start_offset
                    );
                }
                let max_retries = options.max_retries;
                let progress_arc = Arc::clone(&self.progress);
                let mut concurrent_downloader = ConcurrentDownloader::new(
                    Arc::clone(&self.client),
                    self.output_path.clone(),
                    self.request_policy.clone(),
                    cookie_helper.clone(),
                    progress_updater.clone(),
                    Arc::clone(&self.group),
                    progress_arc,
                    self.mmap_threshold,
                    self.file_allocation.clone(),
                    self.global_limiter.clone(),
                );
                match concurrent_downloader.execute_with_retry(
                    uri,
                    total_length,
                    &resume_state,
                    max_retries,
                ).await {
                    Ok(ConcurrentDownloadResult::Complete) => return Ok(()),
                    Ok(ConcurrentDownloadResult::Fallback { completed_ranges }) => {
                        warn!(
                            "Concurrent download falling back to sequential mode, preserving {} completed ranges",
                            completed_ranges.len()
                        );
                        let retry_policy = RetryPolicy::new(options.max_retries, options.retry_wait * 1000);
                        let mut sequential_downloader = SequentialDownloader::new(
                            Arc::clone(&self.client),
                            self.output_path.clone(),
                            self.request_policy.clone(),
                            cookie_helper,
                            progress_updater,
                            Arc::clone(&self.group),
                            Arc::clone(&self.progress),
                            self.global_limiter.clone(),
                        );
                        return sequential_downloader.execute_with_gaps_with_retry(
                            uri,
                            total_length,
                            &completed_ranges,
                            &retry_policy,
                        ).await;
                    }
                    Err(e) => return Err(e),
                }
            }

            let retry_policy = RetryPolicy::new(options.max_retries, options.retry_wait * 1000);
            let mut sequential_downloader = SequentialDownloader::new(
                Arc::clone(&self.client),
                self.output_path.clone(),
                self.request_policy.clone(),
                cookie_helper,
                progress_updater,
                Arc::clone(&self.group),
                Arc::clone(&self.progress),
                self.global_limiter.clone(),
            );
            sequential_downloader.execute_with_retry(
                uri,
                &resume_state,
                total_length,
                &retry_policy,
            ).await
        }
        .await;

        // Update tail reclaim tracking after download attempt completes.
        // In C++ this is called on every data chunk (executeInternal loop);
        // here we update at the boundary since the Rust architecture uses
        // async downloaders that manage their own data loops internally.
        self.update_tail_reclaim_progress();

        if download_result.is_ok() {
            // Verify checksum if configured
            // Extract checksum config before any .await to avoid holding std::sync::RwLockReadGuard across await points
            let checksum_config = {
                let g = self.group.recover();
                g.options().checksum.clone()
            };
            if let Some((ref algo, ref expected)) = checksum_config
                && let Some(ht) = HashType::from_str(algo)
            {
                let cs = Checksum::new(ht, expected)?;
                if !verify_file(&self.output_path, &cs).await? {
                    tracing::error!(
                        algo = %algo,
                        path = %self.output_path.display(),
                        "Checksum mismatch"
                    );
                    return Err(Aria2Error::Checksum(format!(
                        "{} checksum mismatch for {}",
                        algo,
                        self.output_path.display()
                    )));
                }
                tracing::info!(
                    algo = %algo,
                    path = %self.output_path.display(),
                    "Checksum verified successfully"
                );
                {
                    let group = self.group.recover();
                    group.set_checksum_verified(true);
                }
            }
            self.completed = true;
            let g = self.group.recover();
            let total = g.total_length();
            g.update_progress(total);
            g.set_completed_length(total);
        }
        release_path(&self.output_path).await;
        download_result
    }
}

#[async_trait]
impl Command for DownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        // Check for early cancellation (task removed before execution started).
        self.check_cancelled()?;

        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        let uris = self.candidate_uris();
        let first_uri = uris.first().cloned().ok_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "Download URI is empty".into(),
            ))
        })?;

        // MemoryPreDownloadHandler semantics are represented explicitly on
        // the group. Follow options also live on payload groups, so deriving
        // this from DownloadOptions would incorrectly turn a normal payload
        // into an in-memory source download.
        if self.group.recover().is_in_memory_download() {
            return self.execute_in_memory(&first_uri).await;
        }

        // One aggregator belongs to the command generation, not to an
        // individual mirror attempt. Keeping it alive lets progress continue
        // monotonically while a failed resume moves to the next URI.
        self.spawn_progress_aggregator();

        let mut last_error = None;
        let mut candidates = uris.into_iter().peekable();
        while let Some(uri) = candidates.next() {
            match self.execute_attempt(&uri).await {
                Ok(()) => {
                    self.drain_progress_aggregator().await;
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        &error,
                        Aria2Error::Recoverable(crate::error::RecoverableError::CannotResume)
                    ) =>
                {
                    let failure_count = self.group.recover().increase_resume_failure_count();
                    self.group.recover().add_uri_result(
                        uri.clone(),
                        DownloadResultCode::CannotResume.as_code() as u16,
                    );
                    last_error = Some(error);

                    let options = self.group.recover().options_arc();
                    let limit_reached = options.max_resume_failure_tries > 0
                        && failure_count >= options.max_resume_failure_tries;
                    let no_mirror_left = candidates.peek().is_none();

                    if !options.always_resume && (limit_reached || no_mirror_left) {
                        if let Err(reset_error) = self.prepare_fresh_download().await {
                            last_error = Some(reset_error);
                            break;
                        }

                        match self.execute_attempt(&uri).await {
                            Ok(()) => {
                                self.drain_progress_aggregator().await;
                                return Ok(());
                            }
                            Err(error) => last_error = Some(error),
                        }
                        break;
                    }
                }
                Err(error) => {
                    last_error = Some(error);
                    break;
                }
            }
        }

        self.drain_progress_aggregator().await;
        Err(last_error.unwrap_or_else(|| {
            Aria2Error::Fatal(crate::error::FatalError::Config(
                "No download URI is available".into(),
            ))
        }))
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
        Some(Duration::from_secs(
            constants::HTTP_DEFAULT_COMMAND_TIMEOUT_SECS,
        ))
    }
}

impl DownloadCommand {
    fn candidate_uris(&self) -> Vec<String> {
        let mut candidates = Vec::new();
        if !self.initial_uri.is_empty() {
            candidates.push(self.initial_uri.clone());
        }

        let group_uris = self.group.recover().uris().to_vec();
        for uri in group_uris {
            if !uri.is_empty() && !candidates.iter().any(|candidate| candidate == &uri) {
                candidates.push(uri);
            }
        }
        candidates
    }

    /// Reset the shared output for aria2's fresh-download fallback.
    ///
    /// This operation belongs to the command-generation seam: the protocol
    /// downloader reports `CannotResume`, while the command decides whether
    /// the failure means "try another mirror" or "start from byte zero".
    async fn prepare_fresh_download(&mut self) -> Result<()> {
        let control_path =
            crate::filesystem::control_file::ControlFile::control_path_for(&self.output_path);
        match tokio::fs::remove_file(&control_path).await {
            Ok(()) => tracing::debug!(
                path = %control_path.display(),
                "Removed control file before fresh download"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Aria2Error::FileIo(format!(
                    "Failed to reset control file {}: {}",
                    control_path.display(),
                    error
                )));
            }
        }

        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.output_path)
            .await
            .map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to truncate output file {}: {}",
                    self.output_path.display(),
                    error
                ))
            })?;
        file.sync_data().await.map_err(|error| {
            Aria2Error::FileIo(format!(
                "Failed to flush truncated output file {}: {}",
                self.output_path.display(),
                error
            ))
        })?;
        drop(file);

        self.completed_bytes = 0;
        self.progress.set_completed_length(0);
        let group = self.group.recover();
        group.update_progress(0);
        group.set_completed_length(0);
        Ok(())
    }

    /// Download a metadata source into a memory buffer.
    ///
    /// This is the Rust equivalent of aria2's memory pre-download handler:
    /// the response is streamed into an owned `Vec<u8>`, no output path is
    /// opened, and the post-download handler consumes the buffer before the
    /// parent group is demoted.
    async fn execute_in_memory(&mut self, uri: &str) -> Result<()> {
        self.check_cancelled()?;

        // A JSON/session restore may already carry the completed metadata
        // bytes. Reuse them before opening the network source; this preserves
        // `follow-*=mem` across a restart and keeps a completed metadata
        // prerequisite from becoming an unnecessary second download.
        if let Some(data) = self.group.recover().in_memory_data() {
            let completed = data.len() as u64;
            let group = self.group.recover();
            group.set_total_length(completed);
            group.set_completed_length(completed);
            if group.content_type().is_none() {
                group.set_content_type("application/octet-stream");
            }
            group.set_in_memory_data(data);
            drop(group);
            self.completed_bytes = completed;
            self.completed = true;
            self.group.recover_mut().complete()?;
            return Ok(());
        }

        let url = reqwest::Url::parse(uri).ok();
        let cookie_header = url
            .as_ref()
            .map(|url| {
                self.create_cookie_helper()
                    .build_cookie_header_from_url(url)
            })
            .filter(|header| !header.is_empty());

        let request =
            self.request_policy
                .apply(self.client.get(uri), cookie_header.as_deref(), &[]);

        let response = request.send().await.map_err(|error| {
            Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                message: error.to_string(),
            })
        })?;
        let status = response.status();
        if !status.is_success() {
            if status.is_server_error() {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::ServerError {
                        code: status.as_u16(),
                    },
                ));
            }
            return Err(Aria2Error::Recoverable(
                crate::error::RecoverableError::HttpProtocolError {
                    message: format!("HTTP error: {status}"),
                },
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let expected_length = response.content_length().unwrap_or(0);
        let mut data = if expected_length > 0 {
            Vec::with_capacity(expected_length.min(usize::MAX as u64) as usize)
        } else {
            Vec::new()
        };
        let mut stream = response.bytes_stream();
        let mut completed = 0u64;

        while let Some(chunk) = stream.next().await {
            self.check_cancelled()?;
            let chunk = chunk.map_err(|error| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: error.to_string(),
                })
            })?;
            completed = completed.saturating_add(chunk.len() as u64);
            data.extend_from_slice(&chunk);
            self.progress.set_completed_length(completed);
            self.group.recover().update_progress(completed);
        }

        let total_length = if expected_length > 0 {
            expected_length
        } else {
            completed
        };
        let group = self.group.recover();
        group.set_total_length(total_length);
        group.set_completed_length(completed);
        group.mark_in_memory_download();
        if let Some(content_type) = content_type {
            group.set_content_type(content_type);
        }
        group.set_in_memory_data(data);
        drop(group);

        self.completed_bytes = completed;
        self.completed = true;
        self.group.recover_mut().complete()?;
        Ok(())
    }
}
