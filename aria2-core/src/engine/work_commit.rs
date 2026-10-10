//! Shared validation, output, and checkpoint ordering for scheduled work.

use async_trait::async_trait;
use std::sync::{Arc, RwLock};

use crate::engine::progress_checkpoint::ProgressCheckpoint;
use crate::error::Result;
use crate::filesystem::disk_writer::{DiskWriter, SeekableDiskWriter};
use crate::request::request_group::RequestGroup;
use crate::util::rwlock_ext::RwLockRecover;

pub(crate) enum WorkValidation {
    Accepted,
    Rejected,
}

#[derive(Debug)]
pub(crate) enum WorkCommitOutcome {
    Committed,
    Rejected,
}

/// Source-independent commit lifecycle for a completed work item.
///
/// Adapters supply the item's validation rule and storage/checkpoint handles;
/// the core guarantees validation precedes writes and checkpoints only follow
/// successful writes.
#[allow(clippy::double_must_use)]
#[async_trait]
pub(crate) trait WorkCommitter: Send {
    type Output: Send;

    async fn validate(&mut self, output: &mut Self::Output) -> Result<WorkValidation>;

    async fn write(&mut self, output: Self::Output) -> Result<u64>;

    async fn checkpoint(&mut self, committed_bytes: u64) -> Result<()>;
}

pub(crate) async fn commit_work_result<C: WorkCommitter>(
    committer: &mut C,
    mut output: C::Output,
) -> Result<WorkCommitOutcome> {
    match committer.validate(&mut output).await? {
        WorkValidation::Accepted => {}
        WorkValidation::Rejected => return Ok(WorkCommitOutcome::Rejected),
    }

    let committed_bytes = committer.write(output).await?;
    committer.checkpoint(committed_bytes).await?;
    Ok(WorkCommitOutcome::Committed)
}

/// Core-owned sequential output used by streaming data sources.
pub(crate) struct SequentialWorkCommitter<'a> {
    writer: &'a mut dyn DiskWriter,
    completed_bytes: &'a mut u64,
    group: &'a Arc<RwLock<RequestGroup>>,
    checkpoint: &'a mut Option<ProgressCheckpoint>,
}

impl<'a> SequentialWorkCommitter<'a> {
    pub(crate) fn new(
        writer: &'a mut dyn DiskWriter,
        completed_bytes: &'a mut u64,
        group: &'a Arc<RwLock<RequestGroup>>,
        checkpoint: &'a mut Option<ProgressCheckpoint>,
    ) -> Self {
        Self {
            writer,
            completed_bytes,
            group,
            checkpoint,
        }
    }
}

impl SequentialWorkCommitter<'_> {
    pub(crate) async fn commit_chunk(&mut self, output: &[u8]) -> Result<()> {
        self.commit_chunk_with_progress(output, true).await
    }

    pub(crate) async fn commit_chunk_without_progress(&mut self, output: &[u8]) -> Result<()> {
        self.commit_chunk_with_progress(output, false).await
    }

    async fn commit_chunk_with_progress(
        &mut self,
        output: &[u8],
        update_progress: bool,
    ) -> Result<()> {
        self.writer.write(output).await?;
        let committed_bytes = output.len() as u64;
        *self.completed_bytes = self.completed_bytes.saturating_add(committed_bytes);
        if update_progress {
            self.group.recover().update_progress(*self.completed_bytes);
        }
        if let Some(checkpoint) = self.checkpoint.as_mut() {
            let save_requested = self.group.recover().take_save_control_file_request();
            checkpoint
                .update(*self.completed_bytes, save_requested)
                .await;
        }
        Ok(())
    }
}

pub(crate) struct PositionedOutput {
    pub(crate) offset: u64,
    pub(crate) data: bytes::Bytes,
}

/// Core-owned positioned write for work sources that produce offset ranges.
pub(crate) struct PositionedDiskCommitter<'a> {
    writer: &'a mut dyn SeekableDiskWriter,
}

impl<'a> PositionedDiskCommitter<'a> {
    pub(crate) fn new(writer: &'a mut dyn SeekableDiskWriter) -> Self {
        Self { writer }
    }
}

#[async_trait]
impl WorkCommitter for PositionedDiskCommitter<'_> {
    type Output = PositionedOutput;

    async fn validate(&mut self, _output: &mut Self::Output) -> Result<WorkValidation> {
        Ok(WorkValidation::Accepted)
    }

    async fn write(&mut self, output: Self::Output) -> Result<u64> {
        let committed_bytes = output.data.len() as u64;
        self.writer
            .write_bytes_at(output.offset, output.data)
            .await?;
        Ok(committed_bytes)
    }

    async fn checkpoint(&mut self, _committed_bytes: u64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;

    use super::{WorkCommitOutcome, WorkCommitter, WorkValidation, commit_work_result};
    use crate::error::{Aria2Error, Result};

    struct FakeCommitter {
        valid: bool,
        fail_write: bool,
        events: Vec<&'static str>,
    }

    #[async_trait]
    impl WorkCommitter for FakeCommitter {
        type Output = Vec<u8>;

        async fn validate(&mut self, _output: &mut Self::Output) -> Result<WorkValidation> {
            self.events.push("validate");
            Ok(if self.valid {
                WorkValidation::Accepted
            } else {
                WorkValidation::Rejected
            })
        }

        async fn write(&mut self, _output: Vec<u8>) -> Result<u64> {
            self.events.push("write");
            if self.fail_write {
                return Err(Aria2Error::FileIo("fake write failure".into()));
            }
            Ok(4)
        }

        async fn checkpoint(&mut self, _committed_bytes: u64) -> Result<()> {
            self.events.push("checkpoint");
            Ok(())
        }
    }

    #[tokio::test]
    async fn rejected_work_is_neither_written_nor_checkpointed() {
        let mut committer = FakeCommitter {
            valid: false,
            fail_write: false,
            events: Vec::new(),
        };

        let outcome = commit_work_result(&mut committer, b"data".to_vec())
            .await
            .unwrap();

        assert!(matches!(outcome, WorkCommitOutcome::Rejected));
        assert_eq!(committer.events, ["validate"]);
    }

    #[tokio::test]
    async fn accepted_work_is_validated_written_then_checkpointed() {
        let mut committer = FakeCommitter {
            valid: true,
            fail_write: false,
            events: Vec::new(),
        };

        let outcome = commit_work_result(&mut committer, b"data".to_vec())
            .await
            .unwrap();

        assert!(matches!(outcome, WorkCommitOutcome::Committed));
        assert_eq!(committer.events, ["validate", "write", "checkpoint"]);
    }

    #[tokio::test]
    async fn failed_write_never_advances_the_checkpoint() {
        let mut committer = FakeCommitter {
            valid: true,
            fail_write: true,
            events: Vec::new(),
        };

        let error = commit_work_result(&mut committer, b"data".to_vec())
            .await
            .unwrap_err();

        assert!(matches!(error, Aria2Error::FileIo(_)));
        assert_eq!(committer.events, ["validate", "write"]);
    }
}
