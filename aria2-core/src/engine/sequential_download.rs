use std::sync::Arc;

use futures::StreamExt;
use reqwest;

use crate::constants;
use crate::engine::download_cookie::CookieHelper;
use crate::engine::download_progress::ProgressUpdater;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{
    CachedDiskWriter, DefaultDiskWriter, DiskWriter, SeekableDiskWriter,
};
use crate::filesystem::resume_helper::{ResumeHelper, ResumeState};
use crate::http::auth_challenge_handler::{self, AuthChallengeResult};
use crate::http::auth::{AuthConfigFactory, AuthResolveOptions};
use crate::http::conditional_get::SimpleDateTime;
use crate::http::skip_response::{AuthScheme, MAX_REDIRECT_COUNT, RedirectType};
use crate::rate_limiter::{RateLimiter, RateLimiterConfig, ThrottledWriter};
use crate::request::request_group::{AtomicProgress, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

pub struct GapDownloadResult {
    pub completed_gaps: Vec<(u64, u64)>,
    pub error: Option<Aria2Error>,
}

pub struct SequentialDownloader {
    client: Arc<reqwest::Client>,
    output_path: std::path::PathBuf,
    headers: Vec<(String, String)>,
    cookie_helper: CookieHelper,
    progress_updater: ProgressUpdater,
    group: Arc<std::sync::RwLock<RequestGroup>>,
    /// Direct access to progress counters — avoids `RwLock` on the hot path.
    progress: Arc<AtomicProgress>,
}

impl SequentialDownloader {
    pub fn new(
        client: Arc<reqwest::Client>,
        output_path: std::path::PathBuf,
        headers: Vec<(String, String)>,
        cookie_helper: CookieHelper,
        progress_updater: ProgressUpdater,
        group: Arc<std::sync::RwLock<RequestGroup>>,
        progress: Arc<AtomicProgress>,
    ) -> Self {
        Self {
            client,
            output_path,
            headers,
            cookie_helper,
            progress_updater,
            group,
            progress,
        }
    }

    /// Non-blocking cancellation check.
    ///
    /// Returns `Err` when the underlying RequestGroup has been marked
    /// `Removed` (by `aria2.remove` / `aria2.forceRemove`) or `Paused`
    /// (by `aria2.pause` / `aria2.forcePause`). Uses `try_read` on the
    /// outer group lock so it is safe to call from the hot download loop;
    /// a contended lock is treated as "not cancelled" and the caller will
    /// re-check on the next iteration.
    fn check_cancelled(&self) -> Result<()> {
        match self.group.try_read() {
            Ok(g) if g.is_removed() => Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            )),
            Ok(g) if g.is_paused_flag() => {
                Err(Aria2Error::DownloadFailed("Download paused".into()))
            }
            _ => Ok(()),
        }
    }

    /// Attempt an authentication retry when a 401/407 response is received.
    ///
    /// Returns `Some(Ok(()))` if the auth retry succeeded and the download
    /// completed. Returns `Some(Err(...))` if the auth retry failed.
    /// Returns `None` if auth retry is not possible (no credentials,
    /// unsupported scheme, auth already used).
    ///
    /// This mirrors the C++ `HttpSkipResponseCommand::processResponse()` flow
    /// for the 401 case: activate BasicCred → prepareForRetry.
    async fn try_auth_retry(
        &mut self,
        response: &reqwest::Response,
        uri: &str,
        url_parsed: &Option<reqwest::Url>,
        status_code: u16,
        authentication_used: bool,
        resume_state: &ResumeState,
    ) -> Option<Result<()>> {
        let is_proxy = status_code == 407;
        let header_name = if is_proxy { "proxy-authenticate" } else { "www-authenticate" };

        // Extract the auth challenge header
        let auth_header = response.headers().get(header_name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        // Parse the auth scheme
        let scheme = match &auth_header {
            Some(h) => AuthScheme::from_header(h),
            None => {
                // No auth header — if http_auth_challenge is enabled,
                // treat as Basic challenge (matches C++ behavior)
                if !authentication_used {
                    Some(AuthScheme::Basic)
                } else {
                    None
                }
            }
        };

        let scheme = match scheme {
            Some(s) => s,
            None => {
                tracing::warn!(
                    status_code,
                    "Auth challenge received but no supported scheme found"
                );
                return None;
            }
        };

        // Build HttpAuthChallenge from the response
        let challenge = crate::http::skip_response::HttpAuthChallenge {
            scheme: scheme.clone(),
            realm: auth_header.as_deref()
                .map(|h| crate::http::skip_response::HttpSkipResponseHandler::extract_realm(h))
                .unwrap_or_default(),
            is_proxy,
            digest_challenge: if scheme == AuthScheme::Digest {
                auth_header.as_deref()
                    .and_then(|h| crate::http::digest_auth::DigestAuthChallenge::parse(h).ok())
            } else {
                None
            },
        };

        // Resolve auth options from the RequestGroup
        let auth_opts = {
            let g = self.group.recover();
            let opts = g.options();
            AuthResolveOptions {
                http_auth_challenge: opts.http_auth_challenge,
                no_netrc: opts.no_netrc,
                http_user: opts.http_user.clone(),
                http_passwd: opts.http_passwd.clone(),
                ftp_user: opts.ftp_user.clone(),
                ftp_passwd: opts.ftp_passwd.clone(),
            }
        };

        // Only attempt auth if http_auth_challenge is enabled (matches C++ behavior)
        if !auth_opts.http_auth_challenge && scheme != AuthScheme::Digest {
            tracing::debug!(
                status_code,
                "Auth challenge received but http_auth_challenge not enabled"
            );
            return None;
        }

        // Use the URL for credential resolution
        let url = match url_parsed {
            Some(u) => url::Url::parse(&u.to_string()).ok()?,
            None => return None,
        };

        // Resolve credentials via AuthConfigFactory
        let mut auth_factory = AuthConfigFactory::new();
        // Pre-populate from netrc if available
        {
            let g = self.group.recover();
            let opts = g.options();
            if let Some(ref netrc_path) = opts.netrc_path {
                if let Err(e) = auth_factory.load_netrc_file(std::path::Path::new(netrc_path)) {
                    tracing::debug!("Failed to load netrc file {}: {}", netrc_path, e);
                }
            }
        }

        let result = auth_challenge_handler::handle_auth_challenge(
            &challenge,
            &mut auth_factory,
            &url,
            &auth_opts,
            crate::http::request_response::HttpMethod::Get,
            authentication_used,
            1, // nc
        );

        match result {
            AuthChallengeResult::RetryWithAuth {
                authorization_header,
                is_proxy,
            } => {
                // Build the retry request with Authorization header
                // Re-apply the same Range header if we had a resume
                let mut retry_request = if let Some(range_header) = ResumeHelper::build_range_header(resume_state) {
                    tracing::debug!("Auth retry: re-applying Range header: {}", range_header);
                    self.client.get(uri).header("Range", range_header)
                } else {
                    self.client.get(uri)
                };
                if let Some(url) = url_parsed {
                    let cookie_hdr = self.cookie_helper.build_cookie_header_from_url(url);
                    if !cookie_hdr.is_empty() {
                        retry_request = retry_request.header("Cookie", &cookie_hdr);
                    }
                }
                for (name, value) in &self.headers {
                    retry_request = retry_request.header(name, value);
                }

                // Add the Authorization or Proxy-Authorization header
                let header_name = if is_proxy {
                    "Proxy-Authorization"
                } else {
                    "Authorization"
                };
                retry_request = retry_request.header(header_name, &authorization_header);

                tracing::info!(
                    status_code,
                    scheme = ?scheme,
                    "Retrying HTTP request with {} authentication",
                    if is_proxy { "proxy " } else { "" }
                );

                // Send the retry request
                let retry_response = match retry_request.send().await {
                    Ok(r) => r,
                    Err(e) => {
                        return Some(Err(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!("Auth retry request failed: {}", e),
                            },
                        )));
                    }
                };

                self.cookie_helper.extract_and_store_cookies(uri, &retry_response);

                let retry_status = retry_response.status();
                if retry_status.is_success() || retry_status.as_u16() == 206 {
                    // Auth retry succeeded — proceed with the download using
                    // the retry response
                    return Some(self.download_response_body(
                        retry_response,
                        uri,
                        resume_state,
                    ).await);
                }

                // Auth retry still failed
                if retry_status.as_u16() == 401 || retry_status.as_u16() == 407 {
                    tracing::warn!(
                        status_code = retry_status.as_u16(),
                        "Auth retry still failed — credentials may be incorrect"
                    );
                    return Some(Err(Aria2Error::Fatal(
                        crate::error::FatalError::Config("Authentication failed".to_string()),
                    )));
                }

                Some(Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("HTTP error after auth retry: {}", retry_status),
                ))))
            }
            AuthChallengeResult::NoCredentials { status_code, message } => {
                tracing::warn!(
                    status_code,
                    "Auth challenge but no credentials: {}",
                    message
                );
                None // Fall through to normal error handling
            }
            AuthChallengeResult::UnsupportedScheme { scheme, status_code } => {
                tracing::warn!(
                    status_code,
                    scheme,
                    "Unsupported authentication scheme"
                );
                None // Fall through to normal error handling
            }
        }
    }

    pub async fn execute(
        &mut self,
        uri: &str,
        resume_state: &ResumeState,
        total_length: u64,
    ) -> Result<()> {
        #[cfg(not(target_os = "linux"))]
        let _ = total_length;

        #[cfg(target_os = "linux")]
        {
            let no_proxy = {
                let guard = self.group.recover();
                let opts = guard.options();
                opts.http_proxy.is_none() && opts.all_proxy.is_none()
            };
            if !resume_state.should_resume
                && total_length > 0
                && no_proxy
                && !uri.starts_with("https://")
                && self.headers.is_empty()
                && self.cookie_helper.build_cookie_header(uri).is_none()
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
            let mut request = if let Some(range_header) = ResumeHelper::build_range_header(resume_state)
            {
                tracing::debug!("Resume download: {}", range_header);
                self.client.get(&current_uri).header("Range", range_header)
            } else {
                self.client.get(&current_uri)
            };

            if let Some(ref url) = url_parsed {
                let cookie_hdr = self.cookie_helper.build_cookie_header_from_url(url);
                if !cookie_hdr.is_empty() {
                    request = request.header("Cookie", &cookie_hdr);
                }
            }

            for (name, value) in &self.headers {
                request = request.header(name, value);
            }

            // --- Conditional GET: If-Modified-Since header ---
            // Matches C++ HttpRequestCommand L141-171:
            // When `conditional_get` is enabled, protocol is HTTP/HTTPS,
            // the control file does NOT exist, and the output file DOES exist,
            // send the file's mtime as `If-Modified-Since`.
            {
                let g = self.group.recover();
                let opts = g.options();
                if opts.conditional_get
                    && (current_uri.starts_with("http://") || current_uri.starts_with("https://"))
                {
                    let ctrl_path = ControlFile::control_path_for(&self.output_path);
                    if !ctrl_path.exists() && self.output_path.exists() {
                        if let Ok(metadata) = std::fs::metadata(&self.output_path) {
                            if let Ok(modified) = metadata.modified() {
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
                                request = request.header("If-Modified-Since", &http_date);
                            }
                        }
                    }
                }
            }

            let response = request.send().await.map_err(|e| {
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                    message: format!("HTTP request failed: {}", e),
                })
            })?;

            self.cookie_helper.extract_and_store_cookies(&current_uri, &response);

            let status = response.status();
            let status_code = status.as_u16();

            // --- 3xx redirect handling (manual, matching C++ aria2) ---
            // C++ HttpSkipResponseCommand::processResponse() handles redirects
            // by extracting the Location header, resolving the URL, and
            // preparing a retry with the new URI.
            if status.is_redirection() {
                redirect_count += 1;
                if redirect_count > MAX_REDIRECT_COUNT {
                    return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                        format!("Too many redirects: count={}", redirect_count),
                    )));
                }

                // Extract Location header
                let location = response.headers().get("location")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let location = match location {
                    Some(loc) => loc,
                    None => {
                        tracing::warn!(
                            status_code,
                            "Redirect response without Location header"
                        );
                        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                            format!("HTTP {} redirect without Location header", status_code),
                        )));
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
                        return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                            format!("Failed to resolve redirect URL '{}': {}", location, e),
                        )));
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
                if let Some(auth_response) = self.try_auth_retry(
                    &response,
                    &current_uri,
                    &url_parsed,
                    status_code,
                    false, // authentication_used = false (first attempt)
                    resume_state,
                ).await {
                    return auth_response;
                }
                // If try_auth_retry returned None, fall through to error handling
            }

            // --- 304 Not Modified handling (C++ HttpResponseCommand L180) ---
            // When the server returns 304, the file is unchanged since last download.
            // Mark all pieces as done and complete without transferring data.
            if status_code == 304 {
                tracing::info!(
                    "HTTP 304 Not Modified — file unchanged, marking download complete"
                );
                {
                    let g = self.group.recover();
                    // If we know the total length, set it; otherwise use existing
                    if let Some(cl) = response.headers().get("content-length")
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
                if status_code >= 500 {
                    return Err(Aria2Error::Recoverable(RecoverableError::ServerError {
                        code: status_code,
                    }));
                }
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("HTTP error: {}", status),
                )));
            }

            // Proceed with the response body download
            return self.download_response_body(response, &current_uri, resume_state).await;
        }
    }

    /// Download the response body to the output file.
    ///
    /// Extracted from `execute()` so it can be reused by the auth retry path.
    /// Assumes the response status is 2xx or 206.
    async fn download_response_body(
        &mut self,
        response: reqwest::Response,
        _uri: &str,
        resume_state: &ResumeState,
    ) -> Result<()> {

        let resp_length = response.content_length().unwrap_or(0) as u64;
        let actual_total = if resume_state.should_resume {
            resume_state.start_offset + resp_length
        } else {
            resp_length
        };
        {
            let g = self.group.recover();
            g.set_total_length(actual_total);
        }

        let start_offset = if resume_state.should_resume {
            resume_state.start_offset
        } else {
            0
        };

        self.progress_updater.reset(start_offset);

        let rate_limit = { self.group.recover().options().max_download_limit };

        let raw_writer = DefaultDiskWriter::new(&self.output_path);
        let mut writer: Box<dyn DiskWriter> = match rate_limit {
            Some(rate) if rate > 0 => {
                let cfg = RateLimiterConfig::new(Some(rate), None);
                let limiter = RateLimiter::new(&cfg);
                tracing::debug!("Download speed limit enabled: {} bytes/s", rate);
                {
                    let g = self.group.recover();
                    g.set_rate_limiter(limiter.clone());
                }
                Box::new(ThrottledWriter::new(raw_writer, limiter))
            }
            _ => Box::new(raw_writer),
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

        while let Some(chunk) = stream.next().await {
            // Check whether the task was removed between chunks. This is the
            // primary cancellation signal: `aria2.remove` /
            // `aria2.forceRemove` sets the RequestGroup status to `Removed`,
            // which `is_removed()` observes without blocking.
            if let Err(e) = self.check_cancelled() {
                // ADR-0001: Save control file before exiting on pause/remove.
                if let Some(ref mut cf) = ctrl_file {
                    cf.update_completed_length(completed_bytes);
                    if let Err(save_err) = cf.save().await {
                        tracing::warn!(
                            "Sequential: control file save on pause/remove failed: {}",
                            save_err
                        );
                    }
                }
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
                if ctrl_bytes_since_save >= ctrl_save_interval {
                    if let Some(ref mut cf) = ctrl_file {
                        cf.update_completed_length(completed_bytes);
                        if let Err(e) = cf.save().await {
                            tracing::warn!("Sequential: control file save failed: {}", e);
                        }
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

        writer.finalize().await.ok();

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
        // ADR-0001: Delete control file on successful completion.
        drop(ctrl_file);
        if ctrl_path.exists()
            && let Err(e) = tokio::fs::remove_file(&ctrl_path).await {
                tracing::debug!("Failed to delete control file on completion: {}", e);
            }
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }

    pub async fn execute_with_gaps(
        &mut self,
        uri: &str,
        total_length: u64,
        completed_ranges: &[(u64, u64)],
    ) -> GapDownloadResult {
        let gaps = Self::find_all_gaps(completed_ranges, total_length);
        tracing::info!(
            "Starting sequential download with gaps: uri={}, total={}, gaps={:?}",
            uri,
            total_length,
            gaps
        );

        if gaps.is_empty() {
            tracing::info!("No gaps to download, download complete");
            return GapDownloadResult {
                completed_gaps: Vec::new(),
                error: None,
            };
        }

        let url_parsed = reqwest::Url::parse(uri).ok();
        let cookie_hdr = if let Some(ref url) = url_parsed {
            let hdr = self.cookie_helper.build_cookie_header_from_url(url);
            if hdr.is_empty() { None } else { Some(hdr) }
        } else {
            None
        };

        let mut completed_bytes = completed_ranges.iter().map(|(_, len)| len).sum::<u64>();
        self.progress_updater.reset(completed_bytes);

        let mut writer = CachedDiskWriter::new(&self.output_path, Some(total_length), None);

        let rate_limit = { self.group.recover().options().max_download_limit };
        let limiter = rate_limit
            .filter(|&r| r > 0)
            .map(|r| RateLimiter::new(&RateLimiterConfig::new(Some(r), None)));
        if let Some(ref lim) = limiter {
            let g = self.group.recover();
            g.set_rate_limiter(lim.clone());
        }

        let mut last_progress_update = completed_bytes;
        let mut completed_gaps: Vec<(u64, u64)> = Vec::new();

        for (gap_start, gap_length) in gaps {
            // Check whether the task was removed before starting the next gap.
            // This is the primary cancellation signal: `aria2.remove` /
            // `aria2.forceRemove` sets the RequestGroup status to `Removed`,
            // which `is_removed()` observes without blocking.
            if let Err(e) = self.check_cancelled() {
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(e),
                };
            }

            let gap_end = gap_start + gap_length - 1;
            let range_header = format!("bytes={}-{}", gap_start, gap_end);
            tracing::debug!("Sequential Range request for gap: {}", range_header);

            let mut request = self.client.get(uri).header("Range", &range_header);
            if let Some(ref hdr) = cookie_hdr {
                request = request.header("Cookie", hdr);
            }
            for (name, value) in &self.headers {
                request = request.header(name, value);
            }

            let response = match request.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        "Gap download failed ({}), cleaning up partial data",
                        range_header
                    );
                    Self::cleanup_partial_gap(&mut writer, gap_start, 0).await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(Aria2Error::Recoverable(
                            RecoverableError::TemporaryNetworkFailure {
                                message: format!("HTTP request failed: {}", e),
                            },
                        )),
                    };
                }
            };

            self.cookie_helper.extract_and_store_cookies(uri, &response);

            let status = response.status();
            if !status.is_success() && status.as_u16() != 206 {
                tracing::warn!(
                    "Gap download failed with HTTP status {} ({}), cleaning up partial data",
                    status,
                    range_header
                );
                Self::cleanup_partial_gap(&mut writer, gap_start, 0).await;
                let error = if status.as_u16() >= 500 {
                    Aria2Error::Recoverable(RecoverableError::ServerError {
                        code: status.as_u16(),
                    })
                } else {
                    Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                        "HTTP error: {}",
                        status
                    )))
                };
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(error),
                };
            }

            let mut stream = response.bytes_stream();
            let mut stream_offset = gap_start;
            let mut bytes_downloaded = 0u64;

            while let Some(chunk_result) = stream.next().await {
                // Check whether the task was removed between chunks. This is
                // the primary cancellation signal: `aria2.remove` /
                // `aria2.forceRemove` sets the RequestGroup status to
                // `Removed`, which `is_removed()` observes without blocking.
                if let Err(e) = self.check_cancelled() {
                    Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
                    return GapDownloadResult {
                        completed_gaps,
                        error: Some(e),
                    };
                }

                let data: bytes::Bytes = match chunk_result {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(
                            "Gap download failed during streaming ({}), cleaning up partial data",
                            range_header
                        );
                        Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
                        return GapDownloadResult {
                            completed_gaps,
                            error: Some(Aria2Error::Recoverable(
                                RecoverableError::TemporaryNetworkFailure {
                                    message: e.to_string(),
                                },
                            )),
                        };
                    }
                };

                if let Some(ref lim) = limiter {
                    lim.acquire_download(data.len() as u64).await;
                }

                match writer.write_bytes_at(stream_offset, data.clone()).await {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(
                            "Write failed during gap download ({}), cleaning up partial data",
                            range_header
                        );
                        Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
                        return GapDownloadResult {
                            completed_gaps,
                            error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(
                                format!("Write failed: {}", e),
                            ))),
                        };
                    }
                }

                let data_len = data.len() as u64;
                completed_bytes += data_len;
                stream_offset += data_len;
                bytes_downloaded += data_len;

                if completed_bytes - last_progress_update >= constants::PROGRESS_UPDATE_BYTES as u64
                {
                    self.progress_updater
                        .update_progress(
                            completed_bytes,
                            constants::PROGRESS_UPDATE_BYTES as u64,
                            constants::HTTP_SPEED_UPDATE_INTERVAL_MS,
                        )
                        .await;
                    last_progress_update = completed_bytes;
                }
            }

            if bytes_downloaded == gap_length {
                completed_gaps.push((gap_start, gap_length));
                tracing::debug!(
                    "Gap download complete: {} ({} bytes)",
                    range_header,
                    bytes_downloaded
                );
            } else {
                tracing::warn!(
                    "Gap download incomplete: expected {} bytes, got {} bytes",
                    gap_length,
                    bytes_downloaded
                );
                Self::cleanup_partial_gap(&mut writer, gap_start, bytes_downloaded).await;
            }
        }

        if let Err(e) = writer.flush().await {
            tracing::warn!("Flush failed during gap download: {}", e);
            return GapDownloadResult {
                completed_gaps,
                error: Some(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!("Flush failed: {}", e),
                ))),
            };
        }

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
            self.progress.set_total_length(completed_bytes);
            self.progress.set_completed_length(completed_bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            if let Err(e) = g.complete() {
                tracing::warn!("Failed to complete request group: {}", e);
                return GapDownloadResult {
                    completed_gaps,
                    error: Some(e),
                };
            }
        }

        tracing::info!(
            "Sequential download with gaps complete: {} ({} bytes, {} gaps completed)",
            self.output_path.display(),
            completed_bytes,
            completed_gaps.len()
        );
        self.cookie_helper.save_cookies_if_configured();
        GapDownloadResult {
            completed_gaps,
            error: None,
        }
    }

    async fn cleanup_partial_gap(
        writer: &mut CachedDiskWriter,
        gap_start: u64,
        bytes_written: u64,
    ) {
        if bytes_written == 0 {
            return;
        }
        let zero_data = vec![0u8; bytes_written as usize];
        if let Err(e) = writer
            .write_bytes_at(gap_start, bytes::Bytes::from(zero_data))
            .await
        {
            tracing::warn!(
                "Failed to cleanup partial gap at {} ({} bytes): {}",
                gap_start,
                bytes_written,
                e
            );
        }
    }

    pub fn merge_ranges(ranges: &[(u64, u64)]) -> Vec<(u64, u64)> {
        if ranges.is_empty() {
            return Vec::new();
        }

        let mut sorted = ranges.to_vec();
        sorted.sort_by_key(|r| r.0);

        let mut merged = Vec::new();
        let mut current = sorted[0];

        for &(offset, length) in sorted.iter().skip(1) {
            let current_end = current.0 + current.1;
            let next_end = offset + length;

            if offset <= current_end {
                current = (current.0, std::cmp::max(current_end, next_end) - current.0);
            } else {
                merged.push(current);
                current = (offset, length);
            }
        }
        merged.push(current);
        merged
    }

    pub fn find_all_gaps(completed_ranges: &[(u64, u64)], total_length: u64) -> Vec<(u64, u64)> {
        let merged_ranges = Self::merge_ranges(completed_ranges);
        let mut gaps = Vec::new();
        if merged_ranges.is_empty() {
            if total_length > 0 {
                gaps.push((0, total_length));
            }
            return gaps;
        }

        let mut current = 0;
        for &(offset, length) in &merged_ranges {
            if offset > current {
                gaps.push((current, offset - current));
            }
            current = std::cmp::max(current, offset + length);
        }
        if current < total_length {
            gaps.push((current, total_length - current));
        }
        gaps
    }

    pub async fn execute_with_gaps_with_retry(
        &mut self,
        uri: &str,
        total_length: u64,
        completed_ranges: &[(u64, u64)],
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut last_err = None;
        let mut accumulated_completed: Vec<(u64, u64)> = completed_ranges.to_vec();

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                tracing::info!(
                    "Sequential download with gaps retry #{} (waiting {:?}), {} ranges already completed...",
                    attempt,
                    wait,
                    accumulated_completed.len()
                );
                tokio::time::sleep(wait).await;
            }

            let result = self
                .execute_with_gaps(uri, total_length, &accumulated_completed)
                .await;

            if !result.completed_gaps.is_empty() {
                tracing::info!(
                    "Attempt #{} completed {} gaps",
                    attempt + 1,
                    result.completed_gaps.len()
                );
                accumulated_completed.extend(result.completed_gaps);
                accumulated_completed = Self::merge_ranges(&accumulated_completed);
            }

            if result.error.is_none() {
                return Ok(());
            }

            tracing::warn!(
                "Sequential download with gaps attempt #{} failed: {}",
                attempt + 1,
                result.error.as_ref().unwrap()
            );
            last_err = result.error;

            if retry_policy.is_exhausted(attempt)
                || !retry_policy.should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
            {
                break;
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
    }

    pub async fn execute_with_retry(
        &mut self,
        uri: &str,
        resume_state: &ResumeState,
        total_length: u64,
        retry_policy: &RetryPolicy,
    ) -> Result<()> {
        let mut last_err = None;

        for attempt in 0..=retry_policy.max_retries {
            if attempt > 0
                && let Some(wait) = retry_policy.compute_wait(attempt - 1)
            {
                tracing::info!(
                    "Sequential download retry #{} (waiting {:?})...",
                    attempt,
                    wait
                );
                tokio::time::sleep(wait).await;
            }

            match self.execute(uri, resume_state, total_length).await {
                Ok(()) => return Ok(()),
                Err(e) => {
                    tracing::warn!("Sequential download attempt #{} failed: {}", attempt + 1, e);
                    last_err = Some(e);
                    if retry_policy.is_exhausted(attempt)
                        || !retry_policy
                            .should_retry_error(&format!("{:?}", last_err.as_ref().unwrap()))
                    {
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| {
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
                message: "All retries failed".into(),
            })
        }))
    }
}

impl SequentialDownloader {
    #[cfg(target_os = "linux")]
    async fn try_splice_sequential(&mut self, uri: &str, total_length: u64) -> Result<()> {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.output_path)?;

        let bytes = crate::http::splice_http::try_splice_download(uri, 0, total_length, &file, 0)
            .await
            .map_err(|e| Aria2Error::Io(format!("splice download failed: {e}")))?;

        let final_speed = {
            let g = self.group.recover();
            let elapsed = g.elapsed_time();
            match elapsed {
                Some(d) if d.as_secs_f64() > 0.0 => (bytes as f64 / d.as_secs_f64()) as u64,
                _ => 0,
            }
        };

        {
            self.progress.set_total_length(bytes);
            self.progress.set_completed_length(bytes);
            self.progress.set_download_speed(final_speed);
            self.progress.set_upload_speed(0);
            let mut g = self.group.recover_mut();
            g.complete()?;
        }

        tracing::info!(
            "Sequential download (splice) complete: {} ({} bytes)",
            self.output_path.display(),
            bytes
        );
        self.cookie_helper.save_cookies_if_configured();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SequentialDownloader;

    #[test]
    fn test_merge_ranges_empty() {
        let ranges = &[];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert!(result.is_empty());
    }

    #[test]
    fn test_merge_ranges_single() {
        let ranges = &[(0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100)]);
    }

    #[test]
    fn test_merge_ranges_non_overlapping_sorted() {
        let ranges = &[(0, 100), (200, 100), (400, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
    }

    #[test]
    fn test_merge_ranges_non_overlapping_unsorted() {
        let ranges = &[(200, 100), (0, 100), (400, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100), (200, 100), (400, 100)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_inner() {
        let ranges = &[(0, 200), (50, 50)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_inner_unsorted() {
        let ranges = &[(50, 50), (0, 200)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_partial() {
        let ranges = &[(0, 100), (50, 150)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_overlapping_partial_unsorted() {
        let ranges = &[(50, 150), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_adjacent() {
        let ranges = &[(0, 100), (100, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_adjacent_unsorted() {
        let ranges = &[(100, 100), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 200)]);
    }

    #[test]
    fn test_merge_ranges_duplicate() {
        let ranges = &[(0, 100), (0, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 100)]);
    }

    #[test]
    fn test_merge_ranges_multiple_overlapping() {
        let ranges = &[(0, 100), (50, 150), (200, 100), (180, 150)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 330)]);
    }

    #[test]
    fn test_merge_ranges_zero_length() {
        let ranges = &[(0, 0), (100, 0), (200, 100)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 0), (100, 0), (200, 100)]);
    }

    #[test]
    fn test_merge_ranges_complex() {
        let ranges = &[(10, 5), (0, 20), (15, 25), (50, 10), (45, 20), (100, 50)];
        let result = SequentialDownloader::merge_ranges(ranges);
        assert_eq!(result, vec![(0, 40), (45, 20), (100, 50)]);
    }

    #[test]
    fn test_find_all_gaps_empty_ranges() {
        let ranges = &[];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(0, 1000)]);
    }

    #[test]
    fn test_find_all_gaps_no_gaps() {
        let ranges = &[(0, 1000)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert!(gaps.is_empty());
    }

    #[test]
    fn test_find_all_gaps_single_gap() {
        let ranges = &[(0, 500), (600, 400)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(500, 100)]);
    }

    #[test]
    fn test_find_all_gaps_multiple_gaps() {
        let ranges = &[(0, 100), (200, 100), (400, 200)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 1000);
        assert_eq!(gaps, vec![(100, 100), (300, 100), (600, 400)]);
    }

    #[test]
    fn test_find_all_gaps_overlapping_ranges() {
        let ranges = &[(0, 200), (100, 150), (300, 50)];
        let gaps = SequentialDownloader::find_all_gaps(ranges, 500);
        assert_eq!(gaps, vec![(250, 50), (350, 150)]);
    }
}
