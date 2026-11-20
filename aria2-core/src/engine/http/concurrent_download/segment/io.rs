use tokio::sync::mpsc;

use crate::engine::http::segment_downloader::QueuedWriteChunk;
use crate::error::Result;
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::CachedDiskWriter;
use crate::rate_limiter::RateLimiter;

use super::super::ConcurrentDownloader;
use super::super::in_flight::InFlightProgress;
use super::super::range_commit::{HttpRangeCommitOptions, write_http_range_chunk};

pub(super) struct DrainWriteChunksOptions<'a> {
    pub(super) completed_bytes: u64,
    pub(super) in_flight: Option<&'a mut InFlightProgress>,
    pub(super) error_context: &'a str,
}

pub(super) async fn drain_write_chunks(
    downloader: &ConcurrentDownloader,
    write_rx: &mut mpsc::Receiver<QueuedWriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    control_file: &mut Option<ControlFile>,
    mut options: DrainWriteChunksOptions<'_>,
) -> Result<()> {
    while let Ok(chunk) = write_rx.try_recv() {
        write_http_range_chunk(
            downloader,
            writer,
            limiter,
            control_file,
            HttpRangeCommitOptions {
                completed_bytes: options.completed_bytes,
                flush_checkpoint: false,
                error_context: options.error_context,
                in_flight: options.in_flight.as_deref_mut(),
            },
            chunk,
        )
        .await?;
    }
    Ok(())
}
