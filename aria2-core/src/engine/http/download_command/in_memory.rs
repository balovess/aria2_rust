use std::time::Duration;

use crate::engine::retry_policy::RetryPolicy;
use crate::engine::work_runner::{SingleWorkAdapter, run_single_work_item};
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;
use futures::StreamExt;

use super::DownloadCommand;
impl DownloadCommand {
    /// Download a metadata source into a memory buffer, retrying transient
    /// failures according to the original HTTP skip-response contract.
    pub(super) async fn execute_in_memory(&mut self, uri: &str) -> Result<()> {
        let options = self.group.recover().options_arc();
        let retry_policy =
            RetryPolicy::new(options.max_retries, options.retry_wait.saturating_mul(1000));
        let mut work = InMemoryDownloadWork {
            command: self,
            uri,
            retry_policy,
            retry_wait: Duration::from_secs(options.retry_wait),
        };
        run_single_work_item(&mut work).await
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

        let mut current_url = reqwest::Url::parse(uri).map_err(|error| {
            Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                message: format!("invalid HTTP metadata URL {uri}: {error}"),
            })
        })?;
        let mut origin_scope =
            crate::http::request_policy::RedirectOriginScope::new(current_url.clone());
        let cookie_helper = self.create_cookie_helper();
        let mut redirect_count = 0;
        let response = loop {
            self.request_policy.wait_before_request().await;
            let cookie_header = cookie_helper.build_cookie_header_from_url(&current_url);
            let allow_origin_auth = origin_scope.allows_sensitive_headers(&current_url);
            let request = self.request_policy.apply_for_origin(
                self.client.get(current_url.as_str()),
                (!cookie_header.is_empty()).then_some(cookie_header.as_str()),
                &[],
                allow_origin_auth,
            );
            let response = request.send().await.map_err(|error| {
                Aria2Error::Recoverable(crate::error::RecoverableError::TemporaryNetworkFailure {
                    message: error.to_string(),
                })
            })?;
            cookie_helper.extract_and_store_cookies(current_url.as_str(), &response);

            let status_code = response.status().as_u16();
            if !crate::http::response::is_redirect_status(status_code) {
                break response;
            }
            if redirect_count >= crate::http::skip_response::MAX_REDIRECT_COUNT {
                return Err(Aria2Error::Recoverable(
                    crate::error::RecoverableError::HttpTooManyRedirects {
                        count: redirect_count,
                    },
                ));
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| {
                    Aria2Error::Recoverable(crate::error::RecoverableError::HttpProtocolError {
                        message: format!("HTTP {status_code} redirect without Location header"),
                    })
                })?;
            let next_url = current_url.join(location).map_err(|error| {
                Aria2Error::Recoverable(crate::error::RecoverableError::HttpProtocolError {
                    message: format!("failed to resolve redirect URL '{location}': {error}"),
                })
            })?;
            self.group.recover_mut().add_redirect_uri(next_url.as_str());
            origin_scope.follow_redirect(&current_url, &next_url);
            current_url = next_url;
            redirect_count += 1;
        };
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

struct InMemoryDownloadWork<'a> {
    command: &'a mut DownloadCommand,
    uri: &'a str,
    retry_policy: RetryPolicy,
    retry_wait: Duration,
}

#[async_trait::async_trait]
impl SingleWorkAdapter for InMemoryDownloadWork<'_> {
    type Output = ();

    fn max_attempts(&self) -> u32 {
        self.retry_policy.max_tries()
    }

    async fn execute_attempt(&mut self, _attempt: u32) -> Result<Self::Output> {
        self.command.execute_in_memory_attempt(self.uri).await
    }

    fn retry_wait(&self, attempt: u32, error: &Aria2Error) -> Option<Duration> {
        should_retry_in_memory_error(
            error,
            attempt.saturating_sub(1),
            &self.retry_policy,
            self.retry_wait.as_secs(),
            self.command.group.recover().can_retry_file_not_found(),
        )
        .then_some(self.retry_wait)
    }

    async fn wait_for_retry(&mut self, wait: Duration) -> Result<()> {
        if wait.is_zero() {
            return Ok(());
        }
        self.command.wait_for_retry(wait).await
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
