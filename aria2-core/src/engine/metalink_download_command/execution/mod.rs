//! Execution flow for Metalink payloads, metadata, and grouped downloads.

mod file;
mod grouped;
mod metadata;
mod payload;
#[cfg(test)]
mod tests;

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::warn;

use crate::constants;
use crate::engine::command::{Command, CommandStatus};
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::filesystem::disk_writer::DiskWriter;
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use super::MetalinkDownloadCommand;

fn classify_metalink_http_status(status_code: u16) -> Aria2Error {
    if status_code == 404 {
        Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
    } else if status_code == 401 || status_code == 407 {
        Aria2Error::Recoverable(RecoverableError::HttpAuthFailed {
            message: format!("authentication failed: HTTP {status_code}"),
        })
    } else if status_code >= 500 || constants::RETRYABLE_HTTP_CODES.contains(&status_code) {
        Aria2Error::Recoverable(RecoverableError::ServerError { code: status_code })
    } else {
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: format!("HTTP error: {status_code}"),
        })
    }
}

pub(super) struct PayloadDownload {
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
        self.group.recover().timeout()
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

    /// Apply the RequestGroup-owned not-found counter to Metalink HTTP errors.
    ///
    /// Metalink downloads own their mirror loop, so they do not pass through
    /// the ordinary HTTP response command where aria2 increments this counter.
    fn record_not_found_error(&self, error: Aria2Error) -> Aria2Error {
        match error {
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound) => {
                self.group.recover().file_not_found_error()
            }
            error => error,
        }
    }

    /// Return whether a recorded not-found response must stop mirror failover.
    ///
    /// `max-file-not-found=0` disables 404 retries, so the first 404 is
    /// terminal even though its public result remains `ResourceNotFound`.
    /// Positive limits become terminal through `MaxFileNotFound` once the
    /// configured count is reached.
    fn should_stop_after_not_found(&self, error: &Aria2Error) -> bool {
        matches!(
            error,
            Aria2Error::Recoverable(
                RecoverableError::ResourceNotFound | RecoverableError::MaxFileNotFound
            )
        ) && !self.group.recover().can_retry_file_not_found()
    }

    fn should_retry_mirror_error(
        &self,
        attempts: u32,
        error: &Aria2Error,
        retry_policy: &RetryPolicy,
    ) -> bool {
        match error {
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound) => {
                retry_policy.can_retry_after(attempts.saturating_add(1))
                    && self.group.recover().can_retry_file_not_found()
            }
            _ => retry_policy.should_retry(attempts, error),
        }
    }

    async fn wait_for_retry(&self, wait: Duration) -> Result<()> {
        let notifier = self.group.recover().lifecycle_notifier();
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(error) = self.lifecycle_error() {
            return Err(error);
        }

        tokio::select! {
            _ = tokio::time::sleep(wait) => self.lifecycle_error().map_or(Ok(()), Err),
            _ = &mut notified => self.lifecycle_error().map_or(Ok(()), Err),
        }
    }

    async fn download_payload_with_retry(
        &mut self,
        output_path: &Path,
        url: &str,
        expected_size: Option<u64>,
    ) -> Result<PayloadDownload> {
        let options = self.group.recover().options_arc();
        let retry_policy =
            RetryPolicy::new(options.max_retries, options.retry_wait.saturating_mul(1000));
        let mut attempts = 0u32;

        loop {
            match self
                .download_payload_url(output_path, url, expected_size)
                .await
            {
                Ok(payload) => return Ok(payload),
                Err(error) => {
                    let error = self.record_not_found_error(error);
                    if self.lifecycle_error().is_some()
                        || self.should_stop_after_not_found(&error)
                        || !self.should_retry_mirror_error(attempts, &error, &retry_policy)
                    {
                        return Err(error);
                    }

                    attempts = attempts.saturating_add(1);
                    let wait = retry_policy.compute_wait(attempts).unwrap_or_default();
                    warn!(
                        url,
                        attempt = attempts.saturating_add(1),
                        max_attempts = retry_policy.max_tries(),
                        ?wait,
                        error = %error,
                        "Metalink mirror failed, retrying"
                    );
                    self.wait_for_retry(wait).await?;
                }
            }
        }
    }

    async fn wait_for_lifecycle_change(&self) {
        let notifier = self.group.recover().lifecycle_notifier();
        let notified = notifier.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.lifecycle_error().is_none() {
            notified.await;
        }
    }
}
