use tokio::sync::mpsc;

use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::Result;
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::CachedDiskWriter;
use crate::rate_limiter::RateLimiter;

use super::super::ConcurrentDownloader;
use super::super::range_commit::{HttpRangeCommitOptions, write_http_range_chunk};

pub(super) async fn drain_write_chunks(
    downloader: &ConcurrentDownloader,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    control_file: &mut Option<ControlFile>,
    completed_bytes: u64,
    error_context: &str,
) -> Result<()> {
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        write_http_range_chunk(
            downloader,
            writer,
            limiter,
            control_file,
            HttpRangeCommitOptions {
                completed_bytes,
                flush_checkpoint: false,
                error_context,
            },
            WriteChunk { offset, data },
        )
        .await?;
    }
    Ok(())
}
