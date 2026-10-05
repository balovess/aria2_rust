use std::time::Duration;

use crate::engine::retry_policy::RetryPolicy;
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
