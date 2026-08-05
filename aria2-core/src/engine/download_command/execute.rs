use async_trait::async_trait;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::HashType;
use crate::constants;
use crate::engine::active_output_registry::global_registry;
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
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

#[async_trait]
impl Command for DownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        // Check for early cancellation (task removed before execution started).
        self.check_cancelled()?;

        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        let uri = {
            let g = self.group.recover();
            g.uris().first().cloned().unwrap_or_default()
        };

        if uri.is_empty() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "Download URI is empty".into(),
            )));
        }

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

        let original_path = self.output_path.clone();
        self.output_path = global_registry().resolve(&original_path).await;
        if self.output_path != original_path {
            info!(
                "Filename collision resolved: '{}' -> '{}'",
                original_path.display(),
                self.output_path.display()
            );
        }

        let release_path = |path: &std::path::Path| {
            let p = path.to_path_buf();
            #[allow(clippy::let_underscore_future)]
            let _ = tokio::spawn(async move {
                global_registry().release(&p).await;
            });
        };

        let url_for_head = reqwest::Url::parse(&uri).ok();
        let cookie_hdr_head = if let Some(ref url) = url_for_head {
            CookieHelper::new(Arc::clone(&self.cookie_storage), self.cookie_file.clone())
                .build_cookie_header_from_url(url)
        } else {
            String::new()
        };
        let mut head_req = self.client.head(&uri);
        if !cookie_hdr_head.is_empty() {
            head_req = head_req.header("Cookie", &cookie_hdr_head);
        }
        for (name, value) in &self.headers {
            head_req = head_req.header(name, value);
        }
        let head_resp = head_req.send().await.ok();
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
            (0, false)
        };

        let supports_range = if head_supports_range {
            true
        } else if total_length > constants::CONCURRENT_MIN_FILE_SIZE as u64 {
            let prober = RangeProber::new(Arc::clone(&self.client), self.headers.clone())
                .with_cookie_header(
                    url_for_head
                        .as_ref()
                        .map(|url| {
                            self.create_cookie_helper()
                                .build_cookie_header_from_url(url)
                        })
                        .filter(|header| !header.is_empty()),
                );
            prober.probe_range_support(&uri, total_length).await
        } else {
            false
        };

        let resume_helper = ResumeHelper::new(&self.output_path, true);
        let mut resume_state = resume_helper.detect(total_length).await?;

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
            let g = self.group.recover();
            g.set_total_length(self.completed_bytes);
            g.update_progress(self.completed_bytes);
            g.set_completed_length(self.completed_bytes);
            drop(g);
            let mut g = self.group.recover_mut();
            g.complete()?;
            self.completed = true;
            release_path(&self.output_path);
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
            release_path(&self.output_path);
            return Err(e);
        }

        self.spawn_progress_aggregator();

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
                                let mut group = self.group.recover_mut();
                                group.set_completed_length(total_length);
                                group.complete()?;
                                self.completed = true;
                                release_path(&self.output_path);
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

            if self.should_use_concurrent(total_length, supports_range, split) {
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
                    self.headers.clone(),
                    cookie_helper.clone(),
                    progress_updater.clone(),
                    Arc::clone(&self.group),
                    progress_arc,
                    self.mmap_threshold,
                    self.file_allocation.clone(),
                    self.global_limiter.clone(),
                );
                match concurrent_downloader.execute_with_retry(
                    &uri,
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
                            self.headers.clone(),
                            cookie_helper,
                            progress_updater,
                            Arc::clone(&self.group),
                            Arc::clone(&self.progress),
                            self.global_limiter.clone(),
                        );
                        return sequential_downloader.execute_with_gaps_with_retry(
                            &uri,
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
                self.headers.clone(),
                cookie_helper,
                progress_updater,
                Arc::clone(&self.group),
                Arc::clone(&self.progress),
                self.global_limiter.clone(),
            );
            sequential_downloader.execute_with_retry(
                &uri,
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

        self.drain_progress_aggregator().await;

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
                let file = tokio::fs::File::open(&self.output_path)
                    .await
                    .map_err(|e| {
                        Aria2Error::Io(format!(
                            "Failed to open file for checksum verification: {}",
                            e
                        ))
                    })?;
                let mut reader = tokio::io::BufReader::with_capacity(65536, file);
                let mut validator = cs.create_validator();
                let mut buf = vec![0u8; 65536];
                loop {
                    let n = reader.read(&mut buf).await.map_err(|e| {
                        Aria2Error::Io(format!("Read error during checksum verification: {}", e))
                    })?;
                    if n == 0 {
                        break;
                    }
                    validator.update(&buf[..n]);
                }
                if !validator.finalize()? {
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
        release_path(&self.output_path);
        download_result
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
