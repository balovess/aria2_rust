use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::in_flight::InFlightProgress;
use super::{ConcurrentDownloader, flush_requested_control_file};

pub(super) struct HttpRangeCommitOptions<'a> {
    pub(super) completed_bytes: u64,
    pub(super) flush_checkpoint: bool,
    pub(super) error_context: &'a str,
    pub(super) in_flight: Option<&'a mut InFlightProgress>,
}

/// Write one already protocol-checked range chunk into the shared output.
///
/// A range chunk is intermediate data, not a committed work item. The
/// enclosing range scheduler publishes completion only after its persistence
/// barrier; this function therefore does not claim that a successful write is
/// durable.
pub(super) async fn write_http_range_chunk(
    downloader: &ConcurrentDownloader,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    control_file: &mut Option<ControlFile>,
    mut options: HttpRangeCommitOptions<'_>,
    chunk: WriteChunk,
) -> Result<()> {
    let WriteChunk { offset, data } = chunk;
    let data_length = data.len() as u64;
    super::acquire_download_tokens(limiter, downloader.global_limiter.as_ref(), data.len()).await;
    writer.write_bytes_at(offset, data).await.map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Write failed{}: {error}",
            options.error_context
        )))
    })?;

    if let Some(progress) = options.in_flight.as_deref_mut() {
        progress.record_range(offset, data_length);
    }

    if options.flush_checkpoint {
        flush_requested_control_file(
            downloader,
            writer,
            control_file,
            options.completed_bytes,
            options.in_flight.as_deref(),
        )
        .await?;
    }
    Ok(())
}
