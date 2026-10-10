use std::path::Path;

use tokio::sync::mpsc;

use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::{SegmentProgressTracker, WriteChunk};
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::{ConcurrentDownloadResult, ConcurrentDownloader};
use super::io::drain_write_chunks;

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish(
    dl: &mut ConcurrentDownloader,
    should_fallback: bool,
    executor: HttpSegmentRequestExecutor,
    manager: &ConcurrentSegmentManager,
    write_rx: &mut mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    ctrl_file: &mut Option<ControlFile>,
    ctrl_path: &Path,
    completed_bytes: u64,
    progress_tracker: &SegmentProgressTracker,
) -> Result<ConcurrentDownloadResult> {
    if should_fallback {
        executor.cancel().await;
    } else {
        executor.shutdown().await;
    }
    drain_write_chunks(
        dl,
        write_rx,
        writer,
        limiter,
        ctrl_file,
        completed_bytes,
        "",
    )
    .await?;
    writer.sync_all().await.map_err(|error| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Durable output sync failed: {error}"
        )))
    })?;

    if should_fallback {
        let completed_ranges = manager.completed_ranges();
        if let Some(control_file) = ctrl_file.as_mut() {
            control_file.update_completed_length(completed_bytes);
            if let Err(error) = control_file.save().await {
                tracing::warn!("Control file save on fallback failed: {}", error);
            }
        }
        tracing::warn!(
            "Fallback: {} completed ranges will be preserved",
            completed_ranges.len()
        );
        return Ok(ConcurrentDownloadResult::Fallback { completed_ranges });
    }

    let final_speed = {
        let group = dl.group.recover();
        match group.elapsed_time() {
            Some(elapsed) if elapsed.as_secs_f64() > 0.0 => {
                (completed_bytes as f64 / elapsed.as_secs_f64()) as u64
            }
            _ => 0,
        }
    };
    dl.progress.set_completed_length(completed_bytes);
    dl.progress.set_download_speed(final_speed);
    dl.progress.set_upload_speed(0);
    dl.group.recover_mut().complete()?;

    tracing::info!(
        "Concurrent download complete: {} ({} bytes)",
        dl.output_path.display(),
        completed_bytes
    );
    let progress_stats = progress_tracker.stats();
    tracing::debug!(
        segments = progress_stats.segments,
        progress_updates = progress_stats.updates,
        progress_rollbacks = progress_stats.rollbacks,
        "HTTP segment progress aggregation summary"
    );
    drop(ctrl_file.take());
    if ctrl_path.exists()
        && let Err(error) = tokio::fs::remove_file(ctrl_path).await
    {
        tracing::debug!("Failed to delete control file on completion: {}", error);
    }
    dl.cookie_helper.save_cookies_if_configured();
    Ok(ConcurrentDownloadResult::Complete)
}
