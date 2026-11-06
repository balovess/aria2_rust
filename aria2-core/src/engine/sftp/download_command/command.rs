use std::time::Duration;

use async_trait::async_trait;

use crate::engine::command::{Command, CommandStatus};
use crate::engine::work_runner::{SingleWorkAdapter, run_single_work_item};
use crate::error::{Aria2Error, FatalError, RecoverableError, Result};
use crate::filesystem::disk_writer::DiskWriter;
use crate::request::request_group::GroupId;
use crate::util::rwlock_ext::RwLockRecover;

use super::types::SftpDownloadCommand;
#[async_trait]
impl Command for SftpDownloadCommand {
    async fn execute(&mut self) -> Result<()> {
        run_single_work_item(self).await
    }

    /// Return the current status of this command.
    fn status(&self) -> CommandStatus {
        if self.completed_bytes > 0 {
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

    /// Return the timeout for this command.
    fn timeout(&self) -> Option<Duration> {
        self.group.recover().timeout()
    }

    async fn shutdown(&mut self) {
        self.flush_checkpoint().await;
    }
}

#[async_trait]
impl SingleWorkAdapter for SftpDownloadCommand {
    type Output = ();

    fn max_attempts(&self) -> u32 {
        self.retry_policy.max_tries()
    }

    async fn execute_attempt(&mut self, _attempt: u32) -> Result<Self::Output> {
        match self.execute_once().await {
            Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound))
            | Err(Aria2Error::Fatal(FatalError::FileNotFound { .. })) => {
                Err(self.group.recover().file_not_found_error())
            }
            result => result,
        }
    }

    fn retry_wait(&self, attempt: u32, error: &Aria2Error) -> Option<Duration> {
        self.should_retry_error(attempt.saturating_sub(1), error)
            .then(|| self.retry_policy.compute_wait(attempt).unwrap_or_default())
    }

    async fn wait_for_retry(&mut self, wait: Duration) -> Result<()> {
        SftpDownloadCommand::wait_for_retry(self, wait).await
    }

    async fn prepare_retry(&mut self) -> Result<()> {
        self.completed_bytes = 0;
        Ok(())
    }
}

impl SftpDownloadCommand {
    /// Apply the shared total-attempt policy to SFTP failures.
    ///
    /// Remote not-found responses use the separate `max-file-not-found`
    /// counter. Connection, timeout, and other transient transport failures
    /// use the same retry classification as the HTTP and FTP commands.
    pub(super) fn should_retry_error(&self, attempts: u32, error: &Aria2Error) -> bool {
        match error {
            Aria2Error::Recoverable(RecoverableError::ResourceNotFound) => {
                self.retry_policy
                    .can_retry_after(attempts.saturating_add(1))
                    && self.group.recover().can_retry_file_not_found()
            }
            _ => self.retry_policy.should_retry(attempts, error),
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

    pub(super) async fn finalize_partial_writer(&mut self, writer: &mut Box<dyn DiskWriter>) {
        match writer.finalize().await {
            Ok(_) => self.flush_checkpoint().await,
            Err(error) => tracing::warn!(
                %error,
                "SFTP output was not durably finalized; retaining the previous checkpoint"
            ),
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
}
