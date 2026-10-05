use std::collections::HashSet;
use std::time::Duration;

use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{DownloadResultCode, GroupId};
use crate::util::rwlock_ext::RwLockRecover;
use async_trait::async_trait;

use super::DownloadCommand;
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
