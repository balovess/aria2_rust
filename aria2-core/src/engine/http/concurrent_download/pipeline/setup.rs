use std::path::PathBuf;

use crate::constants;
use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::error::{Aria2Error, Result};
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::CachedDiskWriter;
use crate::filesystem::resume_helper::ResumeState;
use crate::rate_limiter::{RateLimiter, RateLimiterConfig};
use crate::request::request_group::DownloadOptions;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::ConcurrentDownloader;
use super::super::fixed_piece_size::calculate_fixed_piece_size;
use super::super::{effective_segment_count, flush_requested_control_file};
use crate::engine::mirror_coordinator::MirrorCoordinator;

pub(super) struct PreparedMultiMirrorDownload {
    pub(super) options: std::sync::Arc<DownloadOptions>,
    pub(super) split: usize,
    /// Immutable parent-piece geometry persisted in the control file.
    pub(super) fixed_piece_size: u64,
    pub(super) max_conn: usize,
    pub(super) session_limit: usize,
    pub(super) coordinator: MirrorCoordinator,
    pub(super) writer: CachedDiskWriter,
    pub(super) limiter: Option<RateLimiter>,
    pub(super) ctrl_path: PathBuf,
    pub(super) ctrl_file: Option<ControlFile>,
    pub(super) ctrl_save_interval: u64,
    pub(super) ctrl_bytes_since_save: u64,
}

pub(super) async fn prepare(
    dl: &mut ConcurrentDownloader,
    uris: &[String],
    total_length: u64,
    resume_state: &ResumeState,
    max_retries_per_segment: u32,
) -> Result<PreparedMultiMirrorDownload> {
    let options = dl.group.recover().options_arc();
    let requested_split = options.split.unwrap_or(constants::DEFAULT_SPLIT);
    let min_split_size = dl.group.recover().effective_min_split_size();
    let split = effective_segment_count(total_length, requested_split, min_split_size);
    let fixed_piece_size = resume_state
        .control_file
        .as_ref()
        .filter(|control_file| {
            control_file.total_length() == total_length && !control_file.is_torrent_checkpoint()
        })
        .and_then(ControlFile::piece_length)
        .map(u64::from)
        .unwrap_or_else(|| calculate_fixed_piece_size(total_length));
    let piece_length = u32::try_from(fixed_piece_size).map_err(|_| {
        Aria2Error::InvalidArgument(format!(
            "HTTP fixed piece length is not representable: {fixed_piece_size}"
        ))
    })?;
    let max_conn = options
        .max_connection_per_server
        .unwrap_or(constants::DEFAULT_MAX_CONNECTION_PER_SERVER as u16)
        .clamp(1, 16) as usize;
    let requested_session_limit = options
        .max_http2_sessions_per_server
        .unwrap_or(constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER as u16)
        .clamp(1, max_conn as u16) as usize;
    let session_limit = requested_session_limit.min(dl.range_clients.len().max(1));

    let mirror_config = crate::engine::mirror_coordinator::MirrorConfig {
        max_connections_per_mirror: split,
        max_total_connections: split,
        speed_threshold: constants::MIRROR_SPEED_THRESHOLD,
        cooldown_secs: constants::MIRROR_COOLDOWN_SECS,
        max_retries: max_retries_per_segment,
    };

    let selector = Box::new(
        crate::selector::adaptive_uri_selector::AdaptiveUriSelector::new_with_uris(
            crate::selector::server_stat_man::ServerStatMan::shared().clone(),
            uris.to_vec(),
        ),
    );
    let segment_manager = ConcurrentSegmentManager::new_with_selector(
        total_length,
        uris.to_vec(),
        Some(fixed_piece_size),
        crate::selector::server_stat_man::ServerStatMan::shared().clone(),
        selector,
    );
    let mut segment_manager = segment_manager;
    // Segment selection may offer any mirror up to the per-download split
    // budget. Authority admission below applies the real server cap and
    // shares it across mirrors with the same scheme/host/port.
    segment_manager.set_max_connections_per_mirror(split);
    let mut coordinator =
        crate::engine::mirror_coordinator::MirrorCoordinator::with_segment_manager(
            crate::selector::server_stat_man::ServerStatMan::shared().clone(),
            segment_manager,
            mirror_config,
            uris.to_vec(),
        );

    let use_mmap = dl.file_allocation == "mmap" && total_length >= dl.mmap_threshold;
    let disk_cache = dl.group.recover().options().disk_cache_size_bytes();
    let mut writer = CachedDiskWriter::new_with_mmap_bytes(
        &dl.output_path,
        Some(total_length),
        disk_cache,
        use_mmap,
    );
    let limiter = dl
        .group
        .recover()
        .options()
        .max_download_limit
        .filter(|&rate| rate > 0)
        .map(|rate| RateLimiter::new(&RateLimiterConfig::new(Some(rate), None)));
    if let Some(ref limiter) = limiter {
        dl.group.recover().set_rate_limiter(limiter.clone());
    }

    let num_pieces = coordinator.num_segments().max(1);
    tracing::info!(
        split_budget = split,
        requested_split,
        min_split_size,
        max_conn,
        fixed_piece_size,
        piece_count = coordinator.num_segments(),
        "Concurrent multi-mirror download started"
    );
    let ctrl_path = ControlFile::control_path_for(&dl.output_path);
    dl.group.recover().set_control_file_path(ctrl_path.clone());
    let expected_bitfield_len = num_pieces.div_ceil(8);
    let persisted_prefix = resume_state
        .control_file
        .as_ref()
        .map(ControlFile::completed_length)
        .filter(|&length| length > 0)
        .or_else(|| {
            (resume_state.control_file.is_none() && resume_state.should_resume)
                .then_some(resume_state.start_offset)
        })
        .unwrap_or(0);
    let compatible_control_file = resume_state.control_file.as_ref().filter(|control_file| {
        control_file.total_length() == total_length
            && !control_file.is_torrent_checkpoint()
            && control_file.piece_length() == Some(piece_length)
            && control_file.bitfield().len() == expected_bitfield_len
    });

    // ResumeHelper is the authority for whether a sidecar belongs to this
    // attempt. In particular, continue=false deliberately returns no control
    // file even when an old sidecar is present; do not let open_or_create()
    // resurrect that state behind the resume seam.
    let has_untrusted_control_file = resume_state.control_file.is_none() && ctrl_path.exists();
    let can_initialize_new_control_file = if compatible_control_file.is_some() {
        true
    } else if has_untrusted_control_file || resume_state.control_file.is_some() {
        match tokio::fs::remove_file(&ctrl_path).await {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                tracing::warn!(
                    path = %ctrl_path.display(),
                    %error,
                    "Failed to replace stale multi-mirror control file"
                );
                false
            }
        }
    } else {
        true
    };
    // A file-length-only resume is safe only when there is no untrusted
    // sidecar left behind. If replacing that sidecar failed, the file and
    // its progress metadata cannot be reconciled, so restart the segments
    // instead of trusting ResumeState::start_offset.
    let can_restore_prefix = resume_state.control_file.is_none()
        && resume_state.should_resume
        && (!has_untrusted_control_file || can_initialize_new_control_file);

    let mut ctrl_file = if let Some(control_file) = compatible_control_file {
        Some(control_file.clone())
    } else if can_initialize_new_control_file {
        if has_untrusted_control_file || resume_state.control_file.is_some() {
            tracing::debug!(
                path = %ctrl_path.display(),
                "Discarding stale control-file layout before multi-mirror resume"
            );
        }
        match ControlFile::open_or_create_with_piece_length(&ctrl_path, total_length, piece_length)
            .await
        {
            Ok(control_file) => Some(control_file),
            Err(error) => {
                tracing::warn!(
                    path = %ctrl_path.display(),
                    %error,
                    "Failed to create multi-mirror control file; resume will be less reliable"
                );
                None
            }
        }
    } else {
        None
    };

    let restored_bytes = if let Some(control_file) = ctrl_file.as_ref() {
        let restored = if compatible_control_file.is_some() && control_file.completed_pieces() > 0 {
            coordinator.restore_completed_from_bitfield(control_file.bitfield())
        } else if compatible_control_file.is_some() || can_restore_prefix {
            coordinator.restore_completed_prefix(persisted_prefix)
        } else {
            0
        };
        if resume_state.should_resume {
            tracing::debug!(
                existing_length = resume_state.existing_length,
                start_offset = resume_state.start_offset,
                restored_bytes = restored,
                "Resuming multi-mirror download from persisted segment state"
            );
        }
        restored
    } else if can_restore_prefix {
        let restored = coordinator.restore_completed_prefix(resume_state.start_offset);
        tracing::debug!(
            existing_length = resume_state.existing_length,
            start_offset = resume_state.start_offset,
            restored_bytes = restored,
            "Resuming multi-mirror download from conservative completed prefix"
        );
        restored
    } else {
        0
    };
    dl.progress_updater.reset(restored_bytes);
    dl.progress.set_completed_length(restored_bytes);

    if let Some(control_file) = ctrl_file.as_mut() {
        control_file.update_completed_length(restored_bytes);
        if let Err(error) = control_file.save().await {
            tracing::warn!(%error, "Failed to save initial multi-mirror control file");
        }
    }
    flush_requested_control_file(
        dl,
        &mut writer,
        &mut ctrl_file,
        coordinator.completed_bytes(),
    )
    .await?;
    let ctrl_save_interval = (total_length / num_pieces as u64).max(1);
    let ctrl_bytes_since_save = 0u64;

    Ok(PreparedMultiMirrorDownload {
        options,
        split,
        fixed_piece_size,
        max_conn,
        session_limit,
        coordinator,
        writer,
        limiter,
        ctrl_path,
        ctrl_file,
        ctrl_save_interval,
        ctrl_bytes_since_save,
    })
}
