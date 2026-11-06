//! FTP download execution logic.
//!
//! Implements the `Command` lifecycle for `FtpDownloadCommand`, including
//! retry handling, cancellation, checkpoints, and address refresh.

use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use tracing::{error, info, warn};

use crate::engine::command::{Command, CommandStatus};
use crate::engine::work_runner::{SingleWorkAdapter, run_single_work_item};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::DiskWriter;
use crate::network::ConnectionContext;
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use super::types::FtpDownloadCommand;

pub(super) struct FtpAttemptError {
    pub(super) source: Aria2Error,
    pub(super) failed_control: Option<ConnectionContext>,
}

impl FtpAttemptError {
    pub(super) fn control(source: Aria2Error, context: ConnectionContext) -> Self {
        Self {
            source,
            failed_control: Some(context),
        }
    }
}

impl From<Aria2Error> for FtpAttemptError {
    fn from(source: Aria2Error) -> Self {
        Self {
            source,
            failed_control: None,
        }
    }
}

#[async_trait]
impl Command for FtpDownloadCommand {
    /// Execute the FTP download with full lifecycle management
    async fn execute(&mut self) -> Result<()> {
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
        }

        info!(
            "FTP download starting: {}:{} -> {}",
            self.host,
            self.port,
            self.output_path.display()
        );

        let in_memory_download = self.group.recover().is_in_memory_download();

        // Create output directory if needed
        if !in_memory_download
            && let Some(parent) = self.output_path.parent()
            && !parent.exists()
        {
            std::fs::create_dir_all(parent).map_err(|e| {
                Aria2Error::Fatal(FatalError::Config(format!("mkdir failed: {}", e)))
            })?;
        }

        run_single_work_item(self).await?;
        info!(
            "FTP download completed successfully: {} ({} bytes)",
            self.output_path.display(),
            self.completed_bytes
        );
        Ok(())
    }

    async fn shutdown(&mut self) {
        self.flush_checkpoint().await;
    }

    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 || self.started {
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

#[async_trait]
impl SingleWorkAdapter for FtpDownloadCommand {
    type Output = ();

    fn max_attempts(&self) -> u32 {
        self.retry_policy.max_tries()
    }

    async fn execute_attempt(&mut self, attempt: u32) -> Result<Self::Output> {
        match self.execute_single_attempt(attempt.saturating_sub(1)).await {
            Ok(()) => Ok(()),
            Err(attempt_error) => {
                self.flush_checkpoint().await;
                let FtpAttemptError {
                    source: mut error,
                    failed_control,
                } = attempt_error;
                if matches!(
                    &error,
                    Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
                ) {
                    error = self.group.recover().file_not_found_error();
                }
                let reject_control = matches!(
                    &error,
                    Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
                        | Aria2Error::Recoverable(RecoverableError::Timeout)
                );
                if reject_control && let Some(context) = failed_control.as_ref() {
                    if let Some(cache) = self.dns_cache.as_ref() {
                        cache.lock().await.mark_bad_context(context);
                    }
                    self.resolved_addresses
                        .retain(|address| *address != context.peer_addr);
                    tracing::debug!(
                        host = %context.endpoint.hostname(),
                        peer = %context.peer_addr,
                        "FTP control connection failed; peer was rejected"
                    );
                }
                Err(error)
            }
        }
    }

    fn retry_wait(&self, attempt: u32, error: &Aria2Error) -> Option<Duration> {
        let should_retry = match error {
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound) => {
                self.retry_policy.can_retry_after(attempt)
                    && self.group.recover().can_retry_file_not_found()
            }
            Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
            | Aria2Error::Recoverable(RecoverableError::Timeout) => {
                self.retry_policy.can_retry_after(attempt)
            }
            _ => false,
        };
        if should_retry {
            let wait = self.retry_policy.compute_wait(attempt).unwrap_or_default();
            warn!(
                "FTP download failed (attempt {}/{}), retrying in {:?}: {}",
                attempt,
                self.retry_policy.max_tries(),
                wait,
                error
            );
            Some(wait)
        } else {
            error!(
                "FTP download failed permanently after {} attempts: {}",
                attempt, error
            );
            None
        }
    }

    async fn wait_for_retry(&mut self, wait: Duration) -> Result<()> {
        FtpDownloadCommand::wait_for_retry(self, wait).await
    }

    async fn prepare_retry(&mut self) -> Result<()> {
        self.completed_bytes = 0;
        Ok(())
    }
}

impl FtpDownloadCommand {
    fn check_cancelled(&self) -> Result<()> {
        let group = self.group.recover();
        if group.is_removed() {
            Err(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            ))
        } else if group.is_paused_flag() {
            Err(Aria2Error::DownloadFailed("Download paused".into()))
        } else if group.is_force_halt_requested() || group.is_halt_requested() {
            Err(Aria2Error::DownloadFailed("FTP download halted".into()))
        } else {
            Ok(())
        }
    }

    /// Wait between retry attempts while still honoring RequestGroup controls.
    /// A plain sleep would delay pause/remove handling for the full configured
    /// retry interval, which can be several minutes.
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

    /// Apply the optional remote timestamp after the output handle has been
    /// finalized. This mirrors the original post-download file-attribute
    /// update while keeping failures non-fatal, as aria2 does.
    pub(super) fn apply_remote_time(
        &self,
        remote_modified_time: Option<SystemTime>,
        in_memory_download: bool,
    ) {
        if in_memory_download {
            return;
        }

        let Some(remote_modified_time) = remote_modified_time else {
            return;
        };

        let result = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.output_path)
            .and_then(|file| file.set_modified(remote_modified_time));
        if let Err(error) = result {
            warn!(
                path = %self.output_path.display(),
                %error,
                "Failed to apply FTP remote modification time"
            );
        }
    }

    pub(super) async fn flush_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.as_mut() {
            let _ = self.group.recover().take_save_control_file_request();
            checkpoint.update(self.completed_bytes, true).await;
        }
    }

    pub(super) async fn complete_checkpoint(&mut self) {
        if let Some(checkpoint) = self.checkpoint.take() {
            checkpoint.complete().await;
        }
    }

    pub(super) async fn finalize_partial_writer(&mut self, writer: &mut Box<dyn DiskWriter>) {
        match writer.finalize().await {
            Ok(_) => self.flush_checkpoint().await,
            Err(error) => tracing::warn!(
                %error,
                "FTP output was not durably finalized; retaining the previous checkpoint"
            ),
        }
    }

    pub(super) async fn refresh_control_addresses(&mut self) -> Result<()> {
        if let Some(cache) = self.dns_cache.as_ref() {
            let mut cache = cache.lock().await;
            let addresses = cache.resolve_with_refresh(&self.host, self.port).await?;
            if addresses.is_empty() {
                return Err(Aria2Error::NameResolve(format!(
                    "No usable address for {}:{}",
                    self.host, self.port
                )));
            }
            self.resolved_addresses = addresses;
            return Ok(());
        }

        self.resolved_addresses = tokio::net::lookup_host((self.host.as_str(), self.port))
            .await
            .map_err(|error| {
                Aria2Error::NameResolve(format!(
                    "DNS resolution failed for {}:{}: {}",
                    self.host, self.port, error
                ))
            })?
            .collect();
        if self.resolved_addresses.is_empty() {
            return Err(Aria2Error::NameResolve(format!(
                "No address resolved for {}:{}",
                self.host, self.port
            )));
        }
        Ok(())
    }
}
