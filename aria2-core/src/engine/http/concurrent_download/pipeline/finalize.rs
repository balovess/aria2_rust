use std::path::Path;

use crate::engine::http::request_executor::HttpSegmentRequestExecutor;
use crate::engine::http::segment_downloader::{SegmentProgressTracker, WriteChunk};
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::rate_limiter::RateLimiter;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::{ConcurrentDownloadResult, ConcurrentDownloader};
use crate::engine::mirror_coordinator::MirrorCoordinator;

#[allow(clippy::too_many_arguments)]
pub(super) async fn finish(
    dl: &mut ConcurrentDownloader,
    should_fallback: bool,
    executor: HttpSegmentRequestExecutor,
    write_rx: &mut tokio::sync::mpsc::Receiver<WriteChunk>,
    writer: &mut CachedDiskWriter,
    limiter: Option<&RateLimiter>,
    coordinator: &MirrorCoordinator,
    ctrl_file: &mut Option<ControlFile>,
    ctrl_path: &Path,
    progress_tracker: &SegmentProgressTracker,
) -> Result<ConcurrentDownloadResult> {
    if should_fallback {
        super::super::segment::cancel_and_persist(
            dl,
            executor,
            write_rx,
            writer,
            limiter,
            ctrl_file,
            coordinator.completed_bytes(),
        )
        .await?;
    } else {
        executor.shutdown().await;
    }
    while let Ok(WriteChunk { offset, data }) = write_rx.try_recv() {
        super::super::range_commit::commit_http_range_chunk(
            dl,
            writer,
            limiter,
            ctrl_file,
            super::super::range_commit::HttpRangeCommitOptions {
                completed_bytes: coordinator.completed_bytes(),
                flush_checkpoint: false,
                error_context: "",
            },
            WriteChunk { offset, data },
        )
        .await?;
    }

    writer.flush().await.map_err(|e| {
        Aria2Error::Fatal(crate::error::FatalError::Config(format!(
            "Flush failed: {}",
            e
        )))
    })?;

    if should_fallback {
        let completed_ranges = coordinator.completed_ranges();
        tracing::warn!(
            "Fallback: {} completed ranges will be preserved",
            completed_ranges.len()
        );
        return Ok(ConcurrentDownloadResult::Fallback { completed_ranges });
    }

    let completed_bytes = coordinator.completed_bytes();
    let final_speed = {
        let g = dl.group.recover();
        let elapsed = g.elapsed_time();
        match elapsed {
            Some(d) if d.as_secs_f64() > 0.0 => (completed_bytes as f64 / d.as_secs_f64()) as u64,
            _ => 0,
        }
    };

    {
        dl.progress.set_total_length(completed_bytes);
        dl.progress.set_completed_length(completed_bytes);
        dl.progress.set_download_speed(final_speed);
        dl.progress.set_upload_speed(0);
        let mut g = dl.group.recover_mut();
        g.complete()?;
    }

    tracing::info!(
        "Multi-mirror concurrent download complete: {} ({} bytes, {} B/s)",
        dl.output_path.display(),
        completed_bytes,
        final_speed
    );
    let progress_stats = progress_tracker.stats();
    tracing::debug!(
        segments = progress_stats.segments,
        progress_updates = progress_stats.updates,
        progress_rollbacks = progress_stats.rollbacks,
        "HTTP multi-mirror progress aggregation summary"
    );
    if let Some(control_file) = ctrl_file.as_mut() {
        control_file.update_completed_length(coordinator.completed_bytes());
        if let Err(error) = control_file.save().await {
            tracing::warn!(%error, "Failed to save final multi-mirror control file");
        }
    }
    drop(ctrl_file.take());
    if ctrl_path.exists()
        && let Err(error) = tokio::fs::remove_file(&ctrl_path).await
    {
        tracing::debug!(%error, "Failed to delete multi-mirror control file on completion");
    }
    dl.cookie_helper.save_cookies_if_configured();
    Ok(ConcurrentDownloadResult::Complete)
}
