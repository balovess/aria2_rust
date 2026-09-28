use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::checksum::checksum::Checksum;
use crate::checksum::message_digest::HashType;
use crate::constants;
use crate::engine::active_output_registry::{OutputPathPolicy, global_registry};
use crate::engine::command::{Command, CommandStatus};
use crate::engine::concurrent_download::{ConcurrentDownloadResult, ConcurrentDownloader};
use crate::engine::download_cookie::CookieHelper;
use crate::engine::range_prober::RangeProber;
use crate::engine::retry_policy::RetryPolicy;
use crate::engine::sequential_download::{PreparedHttpResponse, SequentialDownloader};
use crate::error::{Aria2Error, Result};
use crate::filesystem::file_allocation;
use crate::filesystem::file_allocation_man;
use crate::filesystem::resume_helper::ResumeHelper;
use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::request::HttpMethod;
use crate::http::response_processor::determine_filename_from_response;
use crate::http::{
    AuthChallengeResult, AuthConfigFactory, AuthResolveOptions, AuthScheme, HttpAuthChallenge,
    HttpSkipResponseHandler,
};
use crate::request::request_group::{DownloadResultCode, GroupId};
use crate::util::rwlock_ext::RwLockRecover;

use super::DownloadCommand;

impl DownloadCommand {
    async fn retry_get_after_auth_challenge(
        &self,
        response: reqwest::Response,
        current_url: &reqwest::Url,
        cookie_helper: &CookieHelper,
        auth_factory: &mut AuthConfigFactory,
        auth_options: &AuthResolveOptions,
        authentication_used: bool,
    ) -> reqwest::Response {
        let status_code = response.status().as_u16();
        if status_code != 401 && status_code != 407 {
            return response;
        }

        let is_proxy = status_code == 407;
        let header_name = if is_proxy {
            "proxy-authenticate"
        } else {
            "www-authenticate"
        };
        let auth_header = response
            .headers()
            .get(header_name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let scheme = auth_header
            .as_deref()
            .and_then(AuthScheme::from_header)
            .or((!authentication_used).then_some(AuthScheme::Basic));
        let Some(scheme) = scheme else {
            return response;
        };

        let challenge = HttpAuthChallenge {
            scheme: scheme.clone(),
            realm: auth_header
                .as_deref()
                .map(HttpSkipResponseHandler::extract_realm)
                .unwrap_or_default(),
            is_proxy,
            digest_challenge: (scheme == AuthScheme::Digest)
                .then(|| {
                    auth_header
                        .as_deref()
                        .and_then(|header| DigestAuthChallenge::parse(header).ok())
                })
                .flatten(),
        };

        let AuthChallengeResult::RetryWithAuth {
            authorization_header,
            is_proxy,
        } = crate::http::handle_auth_challenge(
            &challenge,
            auth_factory,
            current_url,
            auth_options,
            HttpMethod::Get,
            authentication_used,
            1,
        )
        else {
            return response;
        };

        let header_name = if is_proxy {
            "Proxy-Authorization"
        } else {
            "Authorization"
        };
        let cookie_header = cookie_helper.build_cookie_header_from_url(current_url);
        let request = self.request_policy.apply(
            self.client.get(current_url.as_str()),
            (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
            &[(header_name.to_string(), authorization_header)],
        );
        let Ok(retry_response) = request.send().await else {
            return response;
        };
        cookie_helper.extract_and_store_cookies(current_url.as_str(), &retry_response);

        // Let the normal response loop handle redirects and HTTP errors. A
        // second auth challenge remains owned by the established downloader.
        if retry_response.status().as_u16() == 401 || retry_response.status().as_u16() == 407 {
            response
        } else {
            retry_response
        }
    }

    async fn send_head_with_redirects(&self, uri: &str) -> Option<PreparedHttpResponse> {
        let mut current_url = reqwest::Url::parse(uri).ok()?;
        let cookie_helper = self.create_cookie_helper();
        let initial_scheme = current_url.scheme().to_owned();
        let options = self.group.recover().options_arc();
        let auth_context = crate::engine::http_auth::from_options(&options, &initial_scheme);
        let (mut auth_factory, auth_options) = (auth_context.factory, auth_context.options);

        for _ in 0..=crate::http::skip_response::MAX_REDIRECT_COUNT {
            let cookie_header = cookie_helper.build_cookie_header_from_url(&current_url);
            let authorization =
                auth_factory.resolve_basic_authorization(&current_url, &auth_options);
            let request = self.request_policy.apply_with_basic_auth(
                self.client.head(current_url.as_str()),
                (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
                &[],
                authorization.as_deref(),
            );
            let response = request.send().await.ok()?;
            cookie_helper.extract_and_store_cookies(current_url.as_str(), &response);

            let status_code = response.status().as_u16();
            if !matches!(status_code, 300..=303 | 307 | 308) {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            }

            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())?;
            current_url = current_url.join(location).ok()?;
        }

        None
    }

    /// Read the first ordinary GET response before finalizing an inferred
    /// output path. The sequential downloader can reuse this response body;
    /// a later concurrent decision may deliberately discard it and issue
    /// range requests instead.
    async fn send_get_with_redirects(&self, uri: &str) -> Option<PreparedHttpResponse> {
        let mut current_url = reqwest::Url::parse(uri).ok()?;
        let cookie_helper = self.create_cookie_helper();
        let initial_scheme = current_url.scheme().to_owned();
        let options = self.group.recover().options_arc();
        let auth_context = crate::engine::http_auth::from_options(&options, &initial_scheme);
        let (mut auth_factory, auth_options) = (auth_context.factory, auth_context.options);

        for _ in 0..=crate::http::skip_response::MAX_REDIRECT_COUNT {
            let cookie_header = cookie_helper.build_cookie_header_from_url(&current_url);
            let authorization =
                auth_factory.resolve_basic_authorization(&current_url, &auth_options);
            let request = self.request_policy.apply_with_basic_auth(
                self.client.get(current_url.as_str()),
                (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
                &[],
                authorization.as_deref(),
            );
            let mut response = request.send().await.ok()?;
            cookie_helper.extract_and_store_cookies(current_url.as_str(), &response);
            response = self
                .retry_get_after_auth_challenge(
                    response,
                    &current_url,
                    &cookie_helper,
                    &mut auth_factory,
                    &auth_options,
                    authorization.is_some(),
                )
                .await;

            if !crate::http::response::is_redirect_status(response.status().as_u16()) {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            }

            let Some(location) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            };

            let Ok(next_url) = current_url.join(location) else {
                return Some(PreparedHttpResponse {
                    response,
                    effective_uri: current_url.to_string(),
                });
            };
            self.group.recover_mut().add_redirect_uri(next_url.as_str());
            current_url = next_url;
        }

        None
    }

    async fn execute_attempt(&mut self, uri: &str) -> Result<()> {
        debug!(
            "Starting download: {} -> {}",
            uri,
            self.output_path.display()
        );

        // Re-check cancellation before the metadata probe and filesystem work
        // so a remove issued before execution is honoured immediately.
        self.check_cancelled()?;

        let release_path = |path: &std::path::Path| {
            let path = path.to_path_buf();
            async move {
                global_registry().release(&path).await;
            }
        };

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
            let prober = RangeProber::new(Arc::clone(&self.client), self.request_policy.clone())
                .with_cookie_header(cookie_header)
                .with_cookie_helper(cookie_helper)
                .with_auth_options(auth_options, options.netrc_path.clone());
            let probe_retry_policy =
                RetryPolicy::new(options.max_retries, options.retry_wait.saturating_mul(1000));
            let mut probe_attempt = 0u32;
            let probe = loop {
                match prober.probe(&effective_uri).await {
                    Ok(probe) => break probe,
                    Err(error) if probe_retry_policy.should_retry(probe_attempt, &error) => {
                        let retry_wait = probe_retry_policy
                            .compute_wait(probe_attempt.saturating_add(1))
                            .unwrap_or_default();
                        probe_attempt = probe_attempt.saturating_add(1);
                        if !retry_wait.is_zero() {
                            self.wait_for_retry(retry_wait).await?;
                        }
                        debug!(
                            attempt = probe_attempt,
                            wait_ms = retry_wait.as_millis() as u64,
                            error = %error,
                            "Retrying transient HTTP Range capability probe"
                        );
                    }
                    Err(error) => return Err(error),
                }
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
            let requested_split = options.split.unwrap_or(constants::DEFAULT_SPLIT);
            let min_split_size = self.group.recover().effective_min_split_size();
            let split = crate::engine::concurrent_download::effective_segment_count(
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
                let total_length = self.group.recover().total_length();
                let verified =
                    crate::checksum::check_integrity::man::enqueue_file_checksum_for_group(
                        &crate::checksum::check_integrity::man::shared(),
                        Arc::clone(&self.group),
                        &self.output_path,
                        total_length,
                        cs,
                    )
                    .await?;
                if !verified {
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

    fn publish_output_path(&self) {
        self.group
            .recover()
            .set_resolved_output_path(self.output_path.to_string_lossy());
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

        let first_uri = self.candidate_uris().into_iter().next().ok_or_else(|| {
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
        let mut attempted_uris = HashSet::new();
        while let Some(uri) = self
            .candidate_uris()
            .into_iter()
            .find(|uri| attempted_uris.insert(uri.clone()))
        {
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
                    let no_mirror_left = self
                        .candidate_uris()
                        .into_iter()
                        .all(|candidate| attempted_uris.contains(&candidate));

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
                    // Re-read the live URI pool before the next attempt.
                    // `aria2.changeUri` mutates the same FileEntry pool that
                    // the original command scheduler observes, so a newly
                    // added mirror must be eligible without recreating the
                    // whole command generation.
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
        self.group.recover().timeout()
    }
}

impl DownloadCommand {
    pub(crate) fn candidate_uris(&self) -> Vec<String> {
        self.group.recover().get_remaining_uris()
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

    /// Download a metadata source into a memory buffer, retrying transient
    /// failures according to the original HTTP skip-response contract.
    async fn execute_in_memory(&mut self, uri: &str) -> Result<()> {
        let options = self.group.recover().options_arc();
        let retry_policy =
            RetryPolicy::new(options.max_retries, options.retry_wait.saturating_mul(1000));
        let mut attempt = 0u32;
        loop {
            match self.execute_in_memory_attempt(uri).await {
                Ok(()) => return Ok(()),
                Err(error)
                    if should_retry_in_memory_error(
                        &error,
                        attempt,
                        &retry_policy,
                        options.retry_wait,
                        self.group.recover().can_retry_file_not_found(),
                    ) =>
                {
                    attempt = attempt.saturating_add(1);
                    if options.retry_wait > 0 {
                        self.wait_for_retry(Duration::from_secs(options.retry_wait))
                            .await?;
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Wait between metadata retries while still honoring RequestGroup
    /// pause, remove, and halt requests.
    pub(super) async fn wait_for_retry(&self, wait: Duration) -> Result<()> {
        let notifier = self.group.recover().lifecycle_notifier();
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        self.check_cancelled()?;
        tokio::select! {
            _ = tokio::time::sleep(wait) => self.check_cancelled(),
            _ = &mut notified => self.check_cancelled(),
        }
    }

    /// Download one metadata source into a memory buffer.
    ///
    /// This is the Rust equivalent of aria2's memory pre-download handler:
    /// the response is streamed into an owned `Vec<u8>`, no output path is
    /// opened, and the post-download handler consumes the buffer before the
    /// parent group is demoted.
    async fn execute_in_memory_attempt(&mut self, uri: &str) -> Result<()> {
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
            if status.as_u16() == 404 {
                return Err(self.group.recover().file_not_found_error());
            }
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
        let lifecycle_notify = self.group.recover().lifecycle_notifier();

        loop {
            let lifecycle_changed = lifecycle_notify.notified();
            tokio::pin!(lifecycle_changed);
            lifecycle_changed.as_mut().enable();
            self.check_cancelled()?;

            let chunk = tokio::select! {
                chunk = stream.next() => chunk,
                _ = lifecycle_changed => {
                    self.check_cancelled()?;
                    continue;
                }
            };
            let Some(chunk) = chunk else {
                break;
            };
            self.check_cancelled()?;
            let chunk = chunk.map_err(|error| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: error.to_string(),
                })
            })?;
            if !chunk.is_empty() {
                // The timeout tracks transport activity, independently of
                // buffering and the coarser displayed progress counter.
                self.progress.record_network_activity();
            }
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

/// Return whether an in-memory HTTP metadata failure should start another
/// request. This deliberately has a narrower status policy than the normal
/// file downloader: it mirrors `HttpSkipResponseCommand` for the metadata
/// pre-download path.
pub(super) fn should_retry_in_memory_error(
    error: &Aria2Error,
    attempt: u32,
    retry_policy: &RetryPolicy,
    retry_wait_secs: u64,
    can_retry_file_not_found: bool,
) -> bool {
    if !retry_policy.can_retry_after(attempt.saturating_add(1)) {
        return false;
    }

    match error {
        Aria2Error::Recoverable(crate::error::RecoverableError::ResourceNotFound) => {
            can_retry_file_not_found
        }
        Aria2Error::Recoverable(
            crate::error::RecoverableError::TemporaryNetworkFailure { .. }
            | crate::error::RecoverableError::Timeout,
        ) => true,
        Aria2Error::Recoverable(crate::error::RecoverableError::ServerError { code }) => match code
        {
            504 => true,
            502 | 503 => retry_wait_secs > 0,
            _ => false,
        },
        _ => false,
    }
}
