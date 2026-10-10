use async_trait::async_trait;

use crate::engine::http::segment_downloader::WriteChunk;
use crate::engine::work_commit::{
    WorkCommitOutcome, WorkCommitter, WorkValidation, commit_work_result,
};
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::{ConcurrentDownloader, flush_requested_control_file};

struct HttpRangeChunkCommitter<'a> {
    downloader: &'a ConcurrentDownloader,
    writer: &'a mut CachedDiskWriter,
    limiter: Option<&'a RateLimiter>,
    control_file: &'a mut Option<ControlFile>,
    completed_bytes: u64,
    flush_checkpoint: bool,
    error_context: &'a str,
}

pub(super) struct HttpRangeCommitOptions<'a> {
    pub(super) completed_bytes: u64,
    pub(super) flush_checkpoint: bool,
    pub(super) error_context: &'a str,
}

#[async_trait]
impl WorkCommitter for HttpRangeChunkCommitter<'_> {
    type Output = WriteChunk;

    async fn validate(&mut self, _output: &mut Self::Output) -> Result<WorkValidation> {
        Ok(WorkValidation::Accepted)
    }

    async fn write(&mut self, output: Self::Output) -> Result<u64> {
        let committed_bytes = output.data.len() as u64;
        super::acquire_download_tokens(
            self.limiter,
            self.downloader.global_limiter.as_ref(),
            output.data.len(),
        )
        .await;
        self.writer
            .write_bytes_at(output.offset, output.data)
            .await
            .map_err(|error| {
                Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                    "Write failed{}: {error}",
                    self.error_context
                )))
            })?;
        Ok(committed_bytes)
    }

    async fn checkpoint(&mut self, _committed_bytes: u64) -> Result<()> {
        if self.flush_checkpoint {
            flush_requested_control_file(
                self.downloader,
                self.writer,
                self.control_file,
                self.completed_bytes,
            )
            .await?;
        }
        Ok(())
    }
}

pub(super) async fn commit_http_range_chunk(
    downloader: &ConcurrentDownloader,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    control_file: &mut Option<ControlFile>,
    options: HttpRangeCommitOptions<'_>,
    chunk: WriteChunk,
) -> Result<()> {
    let mut committer = HttpRangeChunkCommitter {
        downloader,
        writer,
        limiter,
        control_file,
        completed_bytes: options.completed_bytes,
        flush_checkpoint: options.flush_checkpoint,
        error_context: options.error_context,
    };
    match commit_work_result(&mut committer, chunk).await? {
        WorkCommitOutcome::Committed => Ok(()),
        WorkCommitOutcome::Rejected => Err(Aria2Error::Fatal(crate::error::FatalError::Config(
            "HTTP output rejected a range chunk".into(),
        ))),
    }
}
