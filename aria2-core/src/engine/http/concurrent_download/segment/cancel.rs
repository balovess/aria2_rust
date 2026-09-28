use tokio::sync::mpsc;

use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::io::drain_write_chunks;

#[allow(clippy::too_many_arguments)]
pub(in crate::engine::http::concurrent_download) async fn cancel_and_persist(
    executor: HttpSegmentRequestExecutor,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    global_limiter: Option<&RateLimiter>,
    ctrl_file: &mut Option<ControlFile>,
    completed_bytes: u64,
) -> Result<()> {
    executor.cancel().await;
    drain_write_chunks(
        write_rx,
        writer,
        limiter,
        global_limiter,
        " while cancelling",
    )
    .await?;
    writer.flush().await.map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Flush failed while cancelling: {error}"
        )))
    })?;

    if let Some(control_file) = ctrl_file {
        control_file.update_completed_length(completed_bytes);
        if let Err(error) = control_file.save().await {
            tracing::warn!("Control file save on pause/remove failed: {error}");
        }
    }
    Ok(())
}
