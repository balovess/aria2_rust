use tokio::sync::mpsc;

use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::super::acquire_download_tokens;

pub(super) async fn drain_write_chunks(
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    global_limiter: Option<&RateLimiter>,
    error_context: &str,
) -> Result<()> {
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        acquire_download_tokens(limiter, global_limiter, data.len()).await;
        writer.write_bytes_at(offset, data).await.map_err(|error| {
            Aria2Error::Fatal(crate::error::FatalError::Config(format!(
                "Write failed{error_context}: {error}"
            )))
        })?;
    }
    Ok(())
}
