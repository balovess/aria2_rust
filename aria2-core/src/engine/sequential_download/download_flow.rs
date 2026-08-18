// Core sequential download flow: execute() with redirect handling,
// and download_response_body() for streaming the response to disk.

use futures::StreamExt;

use crate::constants;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{DefaultDiskWriter, DiskWriter};
use crate::filesystem::resume_helper::{ResumeHelper, ResumeState};
use crate::http::conditional_get::SimpleDateTime;
use crate::http::response::is_redirect_status;
use crate::http::skip_response::{MAX_REDIRECT_COUNT, RedirectType};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::util::rwlock_ext::RwLockRecover;

use super::SequentialDownloader;
use super::auth_retry::{AuthRetryOutcome, AuthRetryRequest};

fn classify_http_status(status: reqwest::StatusCode) -> Aria2Error {
    let status_code = status.as_u16();
    if status_code >= 500 || constants::RETRYABLE_HTTP_CODES.contains(&status_code) {
        Aria2Error::Recoverable(RecoverableError::ServerError { code: status_code })
    } else {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: format!("HTTP error: {status}"),
        })
    }
}

async fn finalize_cancelled_download(
    writer: &mut Box<dyn DiskWriter>,
    control_file: &mut Option<ControlFile>,
    completed_bytes: u64,
) {
    // Finalize before persisting progress so the control file never claims
    // bytes that are still only buffered in the writer.
    if let Err(error) = writer.finalize().await {
        tracing::warn!("Sequential: finalize on cancellation failed: {}", error);
    }
    if let Some(control_file) = control_file {
        control_file.update_completed_length(completed_bytes);
        if let Err(error) = control_file.save().await {
            tracing::warn!(
                "Sequential: control file save on pause/remove failed: {}",
                error
            );
        }
    }
}

impl SequentialDownloader {
    async fn flush_requested_control_file(
        &self,
        writer: &mut Box<dyn DiskWriter>,
        control_file: &mut Option<ControlFile>,
        completed_bytes: u64,
    ) -> Result<bool> {
        if !self.group.recover().is_save_control_file_requested() {
            return Ok(false);
        }

        writer.flush().await.map_err(|error| {
            Aria2Error::FileIo(format!(
                "Failed to flush requested sequential checkpoint: {error}"
            ))
        })?;
        if let Some(control_file) = control_file {
            control_file.update_completed_length(completed_bytes);
            control_file.save().await.map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to save requested sequential checkpoint: {error}"
                ))
            })?;
        }
        self.group.recover().take_save_control_file_request();
        Ok(true)
    }

    /// Main entry point for sequential HTTP download.
    ///
    /// Handles redirect loop, auth challenge, 304 Not Modified, and
    /// delegates to `download_response_body()` for the actual data transfer.
    pub async fn execute(
        &mut self,
        uri: &str,
        resume_state: &ResumeState,
        total_length: u64,
    ) -> Result<()> {
        // Keep the caller's detection result immutable. A server can reject a
        // resume request with HTTP 200, in which case aria2 either aborts with
        // CANNOT_RESUME. DownloadCommand owns the higher-level decision to
        // try another URI or restart from byte zero.
        let mut effective_resume_state = resume_state.clone();
        let initial_scheme = reqwest::Url::parse(uri)
            .ok()
            .map(|url| url.scheme().to_owned())
            .unwrap_or_else(|| "http".to_string());
        let (mut auth_factory, auth_options) = self.auth_context(&initial_scheme);
        let _has_preemptive_origin_auth = reqwest::Url::parse(uri)
            .ok()
            .and_then(|url| auth_factory.resolve_basic_authorization(&url, &auth_options))
            .is_some();

        #[cfg(not(target_os = "linux"))]
        let _ = total_length;

        #[cfg(target_os = "linux")]
        {
            let no_proxy = {
                let guard = self.group.recover();
                let opts = guard.options();
                opts.http_proxy.is_none() && opts.all_proxy.is_none()
            };
            if !effective_resume_state.should_resume
                && total_length > 0
                && no_proxy
                && !uri.starts_with("https://")
                && !self.request_policy.has_custom_headers()
                && self.cookie_helper.build_cookie_header(uri).is_none()
                && !_has_preemptive_origin_auth
            {
                match self.try_splice_sequential(uri, total_length).await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        tracing::debug!(
                            "Splice download failed for {}, falling back to streaming: {}",
                            uri,
                            e
                        );
                    }
                }
            }
        }

        // Manual redirect loop — matches C++ aria2 behavior where
        // HttpSkipResponseCommand processes 3xx responses and the engine
        // re-issues the request with the new URL. This gives us:
        // - URI selector feedback (redirected URIs are tracked)
        // - Redirect count tracking per-request
        // - Method change rules (301/302/303 → GET, 307/308 → preserve)
        let mut current_uri = uri.to_string();
        let mut redirect_count: u32 = 0;

        loop {
            let url_parsed = reqwest::Url::parse(&current_uri).ok();
            let authorization = url_parsed
                .as_ref()
                .and_then(|url| auth_factory.resolve_basic_authorization(url, &auth_options));
            let base_request = if let Some(range_header) =
                ResumeHelper::build_range_header(&effective_resume_state)
            {
                tracing::debug!("Resume download: {}", range_header);
                self.client.get(&current_uri).header("Range", range_header)
            } else {
                self.client.get(&current_uri)
            };

            // --- Conditional GET: If-Modified-Since header ---
            // Matches C++ HttpRequestCommand L141-171:
            // When `conditional_get` is enabled, protocol is HTTP/HTTPS,
            // the control file does NOT exist, and the output file DOES exist,
            // send the file's mtime as `If-Modified-Since`.
            let mut extra_headers = Vec::new();
            {
                let g = self.group.recover();
                let opts = g.options();
                if opts.conditional_get
                    && (current_uri.starts_with("http://") || current_uri.starts_with("https://"))
                {
                    let ctrl_path = ControlFile::control_path_for(&self.output_path);
                    if !ctrl_path.exists()
                        && self.output_path.exists()
                        && let Ok(metadata) = std::fs::metadata(&self.output_path)
                        && let Ok(modified) = metadata.modified()
                    {
                        let mtime = modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        let dt = SimpleDateTime::from_timestamp(mtime);
                        let http_date = dt.format_imf_fixdate();
                        tracing::debug!(
                            "Conditional GET: sending If-Modified-Since: {}",
                            http_date
                        );
                        extra_headers.push(("If-Modified-Since".to_string(), http_date));
                    }
                }
            }

            let cookie_header = url_parsed
                .as_ref()
                .map(|url| self.cookie_helper.build_cookie_header_from_url(url));
            let request = self.request_policy.apply_with_basic_auth(
                base_request,
                cookie_header.as_deref().filter(|value| !value.is_empty()),
                &extra_headers,
                authorization.as_deref(),
            );
            let conditional_request = self.request_policy.has_header("If-Modified-Since")
                || self.request_policy.has_header("If-None-Match")
                || extra_headers.iter().any(|(name, _)| {
                    name.eq_ignore_ascii_case("If-Modified-Since")
                        || name.eq_ignore_ascii_case("If-None-Match")
                });
            let authentication_used = authorization.is_some()
                || self.request_policy.has_header("Authorization")
                || extra_headers
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case("Authorization"));

            let response = tokio::select! {
                result = request.send() => result.map_err(|e| {
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                        message: format!("HTTP request failed: {}", e),
                    })
                })?,
                cancellation = self.wait_for_cancellation() => {
                    return Err(cancellation.expect_err("cancellation watcher must not complete successfully"));
                }
            };
            self.publish_connection_context(&current_uri, response.remote_addr());

            self.cookie_helper
                .extract_and_store_cookies(&current_uri, &response);

            let status = response.status();
            let status_code = status.as_u16();

            // --- 3xx redirect handling (manual, matching C++ aria2) ---
            // C++ HttpSkipResponseCommand::processResponse() handles redirects
            // by extracting the Location header, resolving the URL, and
            // preparing a retry with the new URI.
            if is_redirect_status(status_code) {
                redirect_count += 1;
                if redirect_count > MAX_REDIRECT_COUNT {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::HttpTooManyRedirects {
                            count: redirect_count,
                        },
                    ));
                }

                // Extract Location header
                let location = response
                    .headers()
                    .get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let location = match location {
                    Some(loc) => loc,
                    None => {
                        tracing::warn!(status_code, "Redirect response without Location header");
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::HttpProtocolError {
                                message: format!(
                                    "HTTP {} redirect without Location header",
                                    status_code
                                ),
                            },
                        ));
                    }
                };

                // Resolve relative URL against the current URL
                let target_url = match &url_parsed {
                    Some(base) => base.join(&location),
                    None => reqwest::Url::parse(&location),
                };

                let target_url = match target_url {
                    Ok(u) => u.to_string(),
                    Err(e) => {
                        return Err(Aria2Error::Recoverable(
                            RecoverableError::HttpProtocolError {
                                message: format!(
                                    "Failed to resolve redirect URL '{}': {}",
                                    location, e
                                ),
                            },
                        ));
                    }
                };

                // Determine redirect type per RFC 7231
                // (Used for method change decisions when we support POST/PUT;
                //  for GET requests, all redirect types preserve the method)
                let _redirect_type = match status_code {
                    300 | 301 => RedirectType::Permanent,
                    303 => RedirectType::SeeOther,
                    307 | 308 => RedirectType::PreserveMethod,
                    _ => RedirectType::Temporary, // 302 and other 3xx
                };

                tracing::info!(
                    status_code,
                    redirect_count,
                    from = %current_uri,
                    to = %target_url,
                    "HTTP redirect"
                );

                // Update RequestGroup URI list with the redirect target
                // (matches C++ behavior where redirected URIs are added to
                // the FileEntry's URI pool for future use)
                {
                    let mut g = self.group.recover_mut();
                    g.add_redirect_uri(&target_url);
                }

                current_uri = target_url;
                // Continue the loop to follow the redirect
                continue;
            }

            // --- Auth challenge handling (401/407) ---
            // Matches C++ HttpSkipResponseCommand::processResponse() 401 flow:
            // If http_auth_challenge is enabled and we haven't already tried auth,
            // attempt to resolve credentials and retry.
            if status_code == 401 || status_code == 407 {
                if let Some(auth_response) = self
                    .try_auth_retry(AuthRetryRequest {
                        response: &response,
                        uri: &current_uri,
                        url_parsed: &url_parsed,
                        status_code,
                        authentication_used,
                        resume_state: &effective_resume_state,
                        auth_factory: &mut auth_factory,
                        auth_opts: &auth_options,
                    })
                    .await
                {
                    match auth_response {
                        Ok(AuthRetryOutcome::Completed(result)) => return result,
                        Ok(AuthRetryOutcome::Redirect(target_url)) => {
                            redirect_count += 1;
                            if redirect_count > MAX_REDIRECT_COUNT {
                                return Err(Aria2Error::Recoverable(
                                    RecoverableError::HttpTooManyRedirects {
                                        count: redirect_count,
                                    },
                                ));
                            }
                            tracing::info!(
                                status_code,
                                redirect_count,
                                from = %current_uri,
                                to = %target_url,
                                "HTTP redirect after authentication"
                            );
                            {
                                let mut g = self.group.recover_mut();
                                g.add_redirect_uri(&target_url);
                            }
                            current_uri = target_url;
                            continue;
                        }
                        Err(error) => return Err(error),
                    }
                }
                // If try_auth_retry returned None, fall through to error handling.
                return Err(Aria2Error::Recoverable(RecoverableError::HttpAuthFailed {
                    message: format!("Authentication failed: HTTP {}", status_code),
                }));
            }

            // --- 304 Not Modified handling (C++ HttpResponseCommand L180) ---
            // When the server returns 304, the file is unchanged since last download.
            // Mark all pieces as done and complete without transferring data.
            if status_code == 304 {
                if !conditional_request {
                    return Err(Aria2Error::Recoverable(
                        RecoverableError::HttpProtocolError {
                            message: "Got 304 without If-Modified-Since or If-None-Match"
                                .to_string(),
                        },
                    ));
                }
                tracing::info!("HTTP 304 Not Modified — file unchanged, marking download complete");
                {
                    let g = self.group.recover();
                    // If we know the total length, set it; otherwise use existing
                    if let Some(cl) = response
                        .headers()
                        .get("content-length")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok())
                    {
                        g.set_total_length(cl);
                    }
                    g.set_completed_length(g.total_length());
                }
                return Ok(());
            }

            if !status.is_success() && status_code != 206 {
                if status_code == 404 {
                    return Err(self.classify_file_not_found());
                }
                return Err(classify_http_status(status));
            }

            // A server which ignores Range returns the complete entity with
            // HTTP 200. C++ aria2 treats that as CANNOT_RESUME unless the
            // caller explicitly opts into a fresh download. Do this before
            // passing the body to the writer; otherwise progress and file
            // offsets would describe a resumed transfer while bytes are
            // actually being written from offset zero.
            if status_code == 200 && Self::resume_requested(&effective_resume_state) {
                effective_resume_state = self
                    .resume_state_after_failed_request(&effective_resume_state)
                    .await?;
            }

            // Proceed with the response body download
            return self
                .download_response_body(response, &current_uri, &effective_resume_state)
                .await;
        }
    }

    pub(in crate::engine::sequential_download) fn resume_requested(state: &ResumeState) -> bool {
        state.should_resume && state.start_offset > 0
    }

    /// Apply aria2's response to an unsupported resume request.
    pub(in crate::engine::sequential_download) async fn resume_state_after_failed_request(
        &self,
        state: &ResumeState,
    ) -> Result<ResumeState> {
        // File allocation may have extended a partial output to the remote
        // length before the server rejected the Range request. Restore the
        // meaningful resume boundary so the next mirror cannot mistake the
        // preallocated tail for completed data.
        let restore_length = state
            .control_file
            .as_ref()
            .map(|control_file| control_file.completed_length())
            .unwrap_or(state.existing_length);
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.output_path)
            .await
            .map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to restore resumable output {}: {}",
                    self.output_path.display(),
                    error
                ))
            })?;
        file.set_len(restore_length).await.map_err(|error| {
            Aria2Error::FileIo(format!(
                "Failed to restore output length for {}: {}",
                self.output_path.display(),
                error
            ))
        })?;
        drop(file);

        // The protocol layer must not choose between another URI and a fresh
        // download. That policy is owned by DownloadCommand, which has the
        // complete mirror list and the group-level retry options.
        Err(Aria2Error::Recoverable(RecoverableError::CannotResume))
    }

    /// Download the response body to the output file.
    ///
    /// Extracted from `execute()` so it can be reused by the auth retry path.
    /// Assumes the response status is 2xx or 206.
    pub(in crate::engine::sequential_download) async fn download_response_body(
        &mut self,
        response: reqwest::Response,
        _uri: &str,
        resume_state: &ResumeState,
    ) -> Result<()> {
        let resp_length = response.content_length().unwrap_or(0);
        let actual_total = if resume_state.should_resume {
            resume_state.start_offset + resp_length
        } else {
            resp_length
        };
        {
            let g = self.group.recover();
            g.set_total_length(actual_total);
        }

        // Extract Last-Modified header for remote-time option.
        // C++ `updateLastModifiedTime()`: when the `remote-time` option is
        // enabled, the file's mtime is set to the server's Last-Modified time
        // after download completion.
        let last_modified = response
            .headers()
            .get("last-modified")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let start_offset = if resume_state.should_resume {
            resume_state.start_offset
        } else {
            0
        };

        self.progress_updater.reset(start_offset);

        let rate_limit = { self.group.recover().options().max_download_limit };

        let raw_writer = if start_offset > 0 {
            DefaultDiskWriter::new_with_offset(&self.output_path, start_offset)
        } else {
            DefaultDiskWriter::new(&self.output_path)
        };

        // Build per-download limiter (if max_download_limit is set).
        let per_limiter = match rate_limit {
            Some(rate) if rate > 0 => {
                let cfg = RateLimiterConfig::new(Some(rate), None);
                let limiter = RateLimiter::new(&cfg);
                tracing::debug!("Download speed limit enabled: {} bytes/s", rate);
                {
                    let g = self.group.recover();
                    g.set_rate_limiter(limiter.clone());
                }
                Some(limiter)
            }
            _ => None,
        };

        // Create ThrottledWriter when either per-download or global limit is active.
        let global_limited = self
            .global_limiter
            .as_ref()
            .is_some_and(|g| g.is_download_limited());
        let mut writer: Box<dyn DiskWriter> = if per_limiter.is_some() || global_limited {
            let limiter = per_limiter.unwrap_or_else(RateLimiter::unlimited);
            let mut tw = ThrottledWriter::new(raw_writer, limiter);
            if let Some(ref gl) = self.global_limiter {
                tw = tw.with_global_limiter(gl.clone());
            }
            Box::new(tw)
        } else {
            Box::new(raw_writer)
        };

        let mut stream = response.bytes_stream();
        let mut completed_bytes = start_offset;
        let write_piece = constants::RATE_LIMITER_CHUNK_SIZE;

        // ADR-0001: Create control file for sequential downloads too.
        // Even without piece-level tracking, the control file's
        // completed_length is the authoritative source for resume detection,
        // immune to the preallocation pitfall.
        let ctrl_path = ControlFile::control_path_for(&self.output_path);
        let mut ctrl_file = if actual_total > 0 {
            match ControlFile::open_or_create(&ctrl_path, actual_total, 1).await {
                Ok(mut cf) => {
                    if start_offset > 0 {
                        cf.update_completed_length(start_offset);
                    }
                    if let Err(e) = cf.save().await {
                        tracing::warn!("Sequential: control file save failed: {}", e);
                    }
                    Some(cf)
                }
                Err(e) => {
                    tracing::warn!(
                        "Sequential: control file creation failed {}: {}",
                        ctrl_path.display(),
                        e
                    );
                    None
                }
            }
        } else {
            None
        };
        let mut ctrl_bytes_since_save: u64 = 0;
        let ctrl_save_interval = (actual_total / 10).max(1024 * 1024); // save every ~10% or 1MB

        let lifecycle_notifier = self.group.recover().lifecycle_notifier();
        loop {
            let next_chunk = {
                let lifecycle_changed = lifecycle_notifier.notified();
                tokio::pin!(lifecycle_changed);
                lifecycle_changed.as_mut().enable();
                tokio::select! {
                    chunk = stream.next() => chunk,
                    _ = &mut lifecycle_changed => {
                        if let Err(error) = self.check_cancelled() {
                            finalize_cancelled_download(
                                &mut writer,
                                &mut ctrl_file,
                                completed_bytes,
                            )
                            .await;
                            return Err(error);
                        }
                        self.flush_requested_control_file(
                            &mut writer,
                            &mut ctrl_file,
                            completed_bytes,
                        )
                        .await?;
                        continue;
                    }
                }
            };
            let Some(chunk) = next_chunk else { break };

            // Check whether the task was removed between chunks. This is the
            // primary cancellation signal: `aria2.remove` /
            // `aria2.forceRemove` sets the RequestGroup status to `Removed`,
            // which `is_removed()` observes without blocking.
            if let Err(e) = self.check_cancelled() {
                finalize_cancelled_download(&mut writer, &mut ctrl_file, completed_bytes).await;
                return Err(e);
            }

            let data: bytes::Bytes = chunk.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: e.to_string(),
                })
            })?;

            let mut offset = 0usize;
            while offset < data.len() {
                let end = (offset + write_piece).min(data.len());
                let piece = &data[offset..end];
                writer.write(piece).await?;
                completed_bytes += piece.len() as u64;
                offset = end;

                // ADR-0001: Periodically update control file with progress.
                ctrl_bytes_since_save += piece.len() as u64;
                let save_requested = self
                    .flush_requested_control_file(&mut writer, &mut ctrl_file, completed_bytes)
                    .await?;
                if let Some(cf) = ctrl_file.as_mut()
                    && (save_requested || ctrl_bytes_since_save >= ctrl_save_interval)
                {
                    cf.update_completed_length(completed_bytes);
                    if let Err(e) = cf.save().await {
                        tracing::warn!("Sequential: control file save failed: {}", e);
                    }
                    ctrl_bytes_since_save = 0;
                }

                self.progress_updater
                    .update_progress(
                        completed_bytes,
                        constants::PROGRESS_UPDATE_BYTES as u64,
                        constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                    )
                    .await;
            }
        }

        writer.finalize().await.map_err(|error| {
            Aria2Error::FileIo(format!("Failed to finalize downloaded file: {error}"))
        })?;

        let final_speed = {
            let g = self.group.recover();
            let elapsed = g.elapsed_time();
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => {
                    (completed_bytes as f64 / d.as_secs_f64()) as u64
                }
                _ => 0,
            }
        };
        {
            self.progress.set_completed_length(completed_bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        tracing::info!(
            "Sequential download complete: {} ({} bytes)",
            self.output_path.display(),
            completed_bytes
        );

        // Apply remote-time: set file mtime to server's Last-Modified.
        // Matches C++ `updateLastModifiedTime()`:
        //   if (getOption()->getAsBool(PREF_REMOTE_TIME)) {
        //     getRequestGroup()->updateLastModifiedTime(lastModified);
        //   }
        // The actual file mtime update happens here, after the file is closed.
        if let Some(ref lm_str) = last_modified {
            let g = self.group.recover();
            if g.options().remote_time {
                // Use the cookie module's RFC 6265 HTTP-date parser which
                // supports IMF-fixdate, RFC 850, and asctime formats.
                if let Some(epoch_secs) = crate::http::cookie::parsing::parse_http_date(lm_str) {
                    let mtime_file =
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch_secs as u64);
                    // Use std::fs::metadata + set_file_mtime via filetime crate
                    // for cross-platform support. If filetime is not available,
                    // we can use platform-specific calls.
                    // For now, use std::fs which supports setting modification time.
                    if let Err(e) = std::fs::File::open(&self.output_path).and_then(|f| {
                        f.set_modified(mtime_file)
                            .map_err(|e2| std::io::Error::new(e2.kind(), e2.to_string()))
                    }) {
                        tracing::warn!(
                            "Failed to set file mtime from Last-Modified '{}': {}",
                            lm_str,
                            e
                        );
                    } else {
                        tracing::debug!("Set file mtime from Last-Modified: {}", lm_str);
                    }
                }
            }
        }

        // ADR-0001: Delete control file on successful completion.
        drop(ctrl_file);
        if ctrl_path.exists()
            && let Err(e) = tokio::fs::remove_file(&ctrl_path).await
        {
            tracing::debug!("Failed to delete control file on completion: {}", e);
        }
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_configured_transient_statuses_as_server_errors() {
        for status_code in [408, 429, 500, 502, 503, 504] {
            let status = reqwest::StatusCode::from_u16(status_code).unwrap();
            assert!(matches!(
                classify_http_status(status),
                Aria2Error::Recoverable(RecoverableError::ServerError { code })
                    if code == status_code
            ));
        }
    }

    #[test]
    fn preserves_non_transient_http_statuses_as_protocol_errors() {
        let status = reqwest::StatusCode::FORBIDDEN;
        assert!(matches!(
            classify_http_status(status),
            Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
                if message.contains("403")
        ));
    }
}
