use tokio::sync::mpsc;

use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::WriteChunk;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;

use super::super::ConcurrentDownloader;
use super::super::in_flight::InFlightProgress;
use super::io::{DrainWriteChunksOptions, drain_write_chunks};

#[allow(clippy::too_many_arguments)]
pub(in crate::engine::http::concurrent_download) async fn cancel_and_persist(
    downloader: &ConcurrentDownloader,
    executor: HttpSegmentRequestExecutor,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    ctrl_file: &mut Option<ControlFile>,
    completed_bytes: u64,
    mut in_flight_progress: Option<&mut InFlightProgress>,
) -> Result<()> {
    executor.cancel().await;
    drain_write_chunks(
        downloader,
        write_rx,
        writer,
        limiter,
        ctrl_file,
        DrainWriteChunksOptions {
            completed_bytes,
            in_flight: in_flight_progress.as_deref_mut(),
            error_context: " while cancelling",
        },
    )
    .await?;
    writer.sync_all().await.map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Durable output sync failed while cancelling: {error}"
        )))
    })?;

    if let Some(control_file) = ctrl_file {
        control_file.update_completed_length(completed_bytes);
        if let Some(progress) = in_flight_progress {
            let pieces = progress.snapshot(control_file.bitfield());
            control_file.set_in_flight_pieces(pieces);
        }
        if let Err(error) = control_file.save().await {
            tracing::warn!("Control file save on pause/remove failed: {error}");
        }
    }
    Ok(())
}
