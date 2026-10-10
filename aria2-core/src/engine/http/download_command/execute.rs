use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::constants;
use crate::engine::active_output_registry::{OutputPathPolicy, global_registry};
use crate::engine::http::concurrent_download::{ConcurrentDownloadResult, ConcurrentDownloader};
use crate::engine::http::segment_downloader::HttpSegmentDownloader;
use crate::engine::http::sequential_download::SequentialDownloader;
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::work_runner::{SingleWorkAdapter, run_single_work_item};
use crate::error::{Aria2Error, Result};
use crate::filesystem::file_allocation;
use crate::filesystem::file_allocation_man;
use crate::filesystem::resume_helper::ResumeHelper;
use crate::http::response_processor::determine_filename_from_response;
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    pub(super) async fn execute_attempt(&mut self, uri: &str) -> Result<()> {
        debug!(
            "Starting download: {} -> {}",
            uri,
            self.output_path.display()
        );

        // Re-check cancellation before the metadata probe and filesystem work
        // so a remove issued before execution is honoured immediately.
        self.check_cancelled()?;

        let options = self.group.recover().options_arc();
        self.publish_output_path();
        let known_total_length = self.group.recover().total_length();
        let needs_metadata_probe =
            options.uses_memory_download() && !options.uses_memory_download_for_uri(uri);
        let should_head = options.dry_run
            || (options.use_head && known_total_length == 0)
            || needs_metadata_probe;
        let head_resp = if should_head {
            self.send_head_with_redirects(uri).await
        } else {
            None
        };
        let mut prepared_get = if !should_head
            && !options.uses_memory_download()
            && !self.output_name_explicit
            && !self.output_path_resolved
        {
            self.send_get_with_redirects(uri).await
        } else {
            None
        };
        let response_for_metadata = prepared_get
            .as_ref()
            .or(head_resp.as_ref())
            .map(|prepared| &prepared.response);
        let mut effective_uri = prepared_get
            .as_ref()
            .or(head_resp.as_ref())
            .map(|prepared| prepared.effective_uri.clone())
            .unwrap_or_else(|| uri.to_owned());

        // An explicit output name is authoritative. For an inferred HTTP
        // name, the first successful response is the metadata seam at which
        // Content-Disposition can replace the URL-derived basename, before
        // collision resolution, resume inspection, or allocation.
        if !self.output_name_explicit
            && !self.output_path_resolved
            && let Some(response) = response_for_metadata
            && response.status().is_success()
        {
            let filename = determine_filename_from_response(
                response,
                options.content_disposition_default_utf8,
            );
            if let Some(parent) = self.output_path.parent() {
                self.output_path = parent.join(filename);
                self.publish_output_path();
            }
        }

        let head_content_type = response_for_metadata.and_then(|response| {
            response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
        });
        if let Some(content_type) = head_content_type {
            self.group.recover().set_content_type(content_type);
            if !options.dry_run && options.uses_memory_download_for_content_type(content_type) {
                self.group.recover().mark_in_memory_download();
                return self.execute_in_memory(uri).await;
            }
        }

        let mut total_length = if let Some(resp) = response_for_metadata {
            resp.headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
        } else {
            0
        };
        if total_length == 0 {
            total_length = known_total_length;
        }

        // `dry_run` is a metadata-only HTTP operation. The HEAD above checks
        // availability and discovers the size, but no range probe, output
        // directory, collision resolution, resume inspection, allocation, or
        // GET request may follow it.
        if options.dry_run {
            self.completed_bytes = total_length;
            {
                let group = self.group.recover();
                group.set_total_length(total_length);
                group.update_progress(total_length);
                group.set_checksum_verified(true);
            }
            self.group.recover_mut().complete()?;
            self.completed = true;
            return Ok(());
        }

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

        let mut supports_range = false;
        let mut probed_http_version = None;
        let metadata_allows_probe = response_for_metadata
            .map(|response| response.status().is_success())
            .unwrap_or(known_total_length > 0);
        let should_probe_range = metadata_allows_probe
            && (total_length == 0 || total_length > constants::CONCURRENT_MIN_FILE_SIZE as u64)
            && (total_length > 0 || should_head || known_total_length > 0);
        if should_probe_range {
            let cookie_helper = self.create_cookie_helper();
            let cookie_header = reqwest::Url::parse(&effective_uri)
                .map(|url| cookie_helper.build_cookie_header_from_url(&url))
                .ok()
                .filter(|header| !header.is_empty());
            let target_scheme = reqwest::Url::parse(&effective_uri)
                .ok()
                .map(|url| url.scheme().to_owned())
                .unwrap_or_else(|| "http".to_owned());
            let (proxy_user, proxy_passwd) = options.proxy_credentials_for_scheme(&target_scheme);
            let auth_options = crate::http::AuthResolveOptions {
                http_auth_challenge: options.http_auth_challenge,
                no_netrc: options.no_netrc,
                http_user: options.http_user.clone(),
                http_passwd: options.http_passwd.clone(),
                ftp_user: options.ftp_user.clone(),
                ftp_passwd: options.ftp_passwd.clone(),
                proxy_user,
                proxy_passwd,
            };
            let prober =
                HttpSegmentDownloader::new_with_policy(&self.client, self.request_policy.clone())
                    .with_cookie_helper(cookie_helper)
                    .with_auth_options(auth_options, options.netrc_path.clone());
            let probe_retry_policy =
                RetryPolicy::new(options.max_retries, options.retry_wait.saturating_mul(1000));
            let probe = {
                let mut work = HttpRangeProbeWork {
                    command: self,
                    prober: &prober,
                    uri: &effective_uri,
                    cookie_header: cookie_header.as_deref(),
                    retry_policy: probe_retry_policy,
                };
                run_single_work_item(&mut work).await?
            };
            supports_range = probe.supports_range;
            if probe.total_length > 0 {
                total_length = probe.total_length;
            }
            effective_uri = probe.effective_url;
            if effective_uri != uri {
                self.group
                    .recover_mut()
                    .add_redirect_uri(effective_uri.as_str());
            }
            info!(
                supports_range,
                total_length,
                final_url = %effective_uri,
                http_version = ?probe.version,
                "HTTP range capability probe completed"
            );
            probed_http_version = probe.version;
        }

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
        if !self.output_path_resolved {
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
            self.output_path_resolved = true;
        } else {
            // A mirror failover re-enters this attempt with the same command.
            // Reclaim the already-resolved path instead of applying the
            // filesystem collision policy a second time and renaming the
            // partial/preallocated output.
            self.output_path = global_registry().resolve(&self.output_path).await;
        }
        if self.output_path != original_path {
            info!(
                "Filename collision resolved: '{}' -> '{}'",
                original_path.display(),
                self.output_path.display()
            );
        }
        self.publish_output_path();

        let continue_download = options.continue_download;
        let resume_helper = ResumeHelper::new(&self.output_path, continue_download);
        let mut resume_state = match resume_helper.detect(total_length).await {
            Ok(state) => state,
            Err(error) => {
                self.release_output_path().await;
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
            self.release_output_path().await;
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
            self.release_output_path().await;
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
                use crate::checksum::check_integrity::{
                    man as ci_man, IntegrityTrailingGarbageAction,
                };
                IntegrityTrailingGarbageAction::single_file(
                    self.output_path.clone(),
                    total_length,
                )
                .apply()
                .await?;
                use crate::checksum::message_digest::HashType;
                // Extract owned data first so the RwLock guard is dropped
                // before any await (guard is not Send).
                let piece_info = self
                    .group
                    .recover()
                    .get_download_context()
                    .map(|ctx| {
                        (
                            ctx.get_piece_hashes()
                                .iter()
                                .map(|hash| hash.to_string())
                                .collect(),
                            ctx.get_piece_length() as u64,
                            ctx.get_piece_hash_type().to_string(),
                        )
                    });
                if let Some((hashes, piece_len, hash_type)) = piece_info {
                    let algo = if hash_type.is_empty() {
                        // Empty piece-hash metadata is the legacy SHA-1
                        // default used by BitTorrent-style contexts.
                        HashType::Sha1
                    } else {
                        HashType::from_str(&hash_type).ok_or_else(|| {
                            Aria2Error::Parse(format!(
                                "unknown piece hash algorithm: {hash_type}"
                            ))
                        })?
                    };
                    if let Some(task) = ci_man::file_task(
                        &self.output_path,
                        piece_len.max(1),
                        total_length,
                        hashes,
                        algo,
                    )? {
                        let gid = self.group.recover().gid().value();
                        info!(gid, "Checking integrity of existing data against piece hashes");
                        let outcome = ci_man::enqueue_with_outcome_for_group(
                            &ci_man::shared(),
                            Arc::clone(&self.group),
                            task,
                        )
                        .await?;
                        let ok = outcome.verified;
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
                                self.release_output_path().await;
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
            let requested_split = options.split.unwrap_or(constants::DEFAULT_SPLIT);
            let min_split_size = self.group.recover().effective_min_split_size();
            let split = crate::engine::http::concurrent_download::effective_segment_count(
                total_length,
                requested_split,
                min_split_size,
            ) as u16;

            let cookie_helper = self.create_cookie_helper();
            let progress_updater = self.create_progress_updater();

            if self.should_use_concurrent(total_length, supports_range, split)
                && !options.http_accept_gzip
            {
                // A full prepared response cannot satisfy segmented range
                // requests. Drop it before entering the concurrent adapter.
                let _ = prepared_get.take();
                if resume_state.should_resume {
                    info!(
                        "Concurrent mode + resume: existing {} bytes, continuing from offset {}",
                        resume_state.existing_length, resume_state.start_offset
                    );
                }
                let max_retries = options.max_retries;
                let target_scheme = reqwest::Url::parse(&effective_uri)
                    .ok()
                    .map(|url| url.scheme().to_owned())
                    .unwrap_or_else(|| "http".to_string());
                let (proxy_user, proxy_passwd) = options.proxy_credentials_for_scheme(
                    &target_scheme,
                );
                let auth_options = crate::http::AuthResolveOptions {
                    http_auth_challenge: options.http_auth_challenge,
                    no_netrc: options.no_netrc,
                    http_user: options.http_user.clone(),
                    http_passwd: options.http_passwd.clone(),
                    ftp_user: options.ftp_user.clone(),
                    ftp_passwd: options.ftp_passwd.clone(),
                    proxy_user,
                    proxy_passwd,
                };
                let progress_arc = Arc::clone(&self.progress);
                let mut concurrent_downloader = ConcurrentDownloader::new(
                    Arc::clone(&self.client),
                    self.output_path.clone(),
                    self.request_policy.clone(),
                    auth_options,
                    options.netrc_path.clone(),
                    cookie_helper.clone(),
                    progress_updater.clone(),
                    Arc::clone(&self.group),
                    progress_arc,
                    self.mmap_threshold,
                    self.file_allocation.clone(),
                    self.global_limiter.clone(),
                )
                .with_range_clients(Arc::clone(&self.range_clients))
                .with_initial_http_version(&effective_uri, probed_http_version);
                match concurrent_downloader.execute_with_retry(
                    uri,
                    &effective_uri,
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
                        )
                        .with_outbound_network_policy(Arc::clone(
                            &self.outbound_network_policy,
                        ));
                        let result = sequential_downloader.execute_with_gaps_with_retry(
                            uri,
                            total_length,
                            &completed_ranges,
                            &retry_policy,
                        ).await;
                        drop(sequential_downloader);
                        return result;
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
            )
            .with_outbound_network_policy(Arc::clone(&self.outbound_network_policy));
            if let Some(prepared) = prepared_get.take() {
                sequential_downloader = sequential_downloader.with_prepared_response(prepared);
            }
            let result = sequential_downloader.execute_with_retry(
                uri,
                &resume_state,
                total_length,
                &retry_policy,
            ).await;
            drop(sequential_downloader);
            result
        }
        .await;

        self.finalize_attempt(download_result).await
    }

    fn publish_output_path(&self) {
        self.group
            .recover()
            .set_resolved_output_path(self.output_path.to_string_lossy());
    }
}

struct HttpRangeProbeWork<'a> {
    command: &'a mut DownloadCommand,
    prober: &'a HttpSegmentDownloader,
    uri: &'a str,
    cookie_header: Option<&'a str>,
    retry_policy: RetryPolicy,
}

#[async_trait::async_trait]
impl SingleWorkAdapter for HttpRangeProbeWork<'_> {
    type Output = crate::engine::http::segment_downloader::RangeProbeResult;

    fn max_attempts(&self) -> u32 {
        self.retry_policy.max_tries()
    }

    async fn execute_attempt(&mut self, _attempt: u32) -> Result<Self::Output> {
        self.prober
            .probe_range_metadata(self.uri, self.cookie_header)
            .await
    }

    fn retry_wait(&self, attempt: u32, error: &Aria2Error) -> Option<std::time::Duration> {
        if !self
            .retry_policy
            .should_retry(attempt.saturating_sub(1), error)
        {
            return None;
        }
        let wait = self.retry_policy.compute_wait(attempt).unwrap_or_default();
        debug!(
            attempt,
            wait_ms = wait.as_millis() as u64,
            error = %error,
            "Retrying transient HTTP Range capability probe"
        );
        Some(wait)
    }

    async fn wait_for_retry(&mut self, wait: std::time::Duration) -> Result<()> {
        if wait.is_zero() {
            return Ok(());
        }
        self.command.wait_for_retry(wait).await
    }
}
