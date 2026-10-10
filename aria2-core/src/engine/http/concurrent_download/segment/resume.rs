use std::path::PathBuf;

use crate::engine::concurrent_segment_manager::ConcurrentSegmentManager;
use crate::error::Result;
use crate::filesystem::control_file::ControlFile;
use crate::filesystem::disk_writer::{CachedDiskWriter, SeekableDiskWriter};
use crate::filesystem::resume_helper::ResumeState;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::{ConcurrentDownloader, flush_requested_control_file};

pub(super) struct PreparedResume {
    pub(super) ctrl_path: PathBuf,
    pub(super) ctrl_file: Option<ControlFile>,
    pub(super) ctrl_save_interval: u64,
    pub(super) completed_bytes: u64,
}

pub(super) async fn prepare(
    dl: &mut ConcurrentDownloader,
    manager: &mut ConcurrentSegmentManager,
    resume_state: &ResumeState,
    total_length: u64,
    piece_length: u32,
    writer: &mut CachedDiskWriter,
) -> Result<PreparedResume> {
    // Persist completed pieces so pause-resume can distinguish a preallocated
    // file from a completed download.
    let num_pieces = manager.num_segments().max(1);
    let ctrl_path = ControlFile::control_path_for(&dl.output_path);
    dl.group.recover().set_control_file_path(ctrl_path.clone());
    let expected_bitfield_len = num_pieces.div_ceil(8);
    let compatible_control_file = resume_state.control_file.as_ref().filter(|control_file| {
        control_file.total_length() == total_length
            && !control_file.is_torrent_checkpoint()
            && control_file.piece_length() == Some(piece_length)
            && control_file.bitfield().len() == expected_bitfield_len
    });
    let has_untrusted_control_file = resume_state.control_file.is_none() && ctrl_path.exists();
    let can_initialize_control_file = if compatible_control_file.is_some() {
        true
    } else if has_untrusted_control_file || resume_state.control_file.is_some() {
        match tokio::fs::remove_file(&ctrl_path).await {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => {
                tracing::warn!(
                    path = %ctrl_path.display(),
                    %error,
                    "Failed to replace stale single-source control file"
                );
                false
            }
        }
    } else {
        true
    };
    let can_restore_prefix = resume_state.control_file.is_none()
        && resume_state.should_resume
        && (!has_untrusted_control_file || can_initialize_control_file);
    let mut ctrl_file = if let Some(control_file) = compatible_control_file {
        Some(control_file.clone())
    } else if can_initialize_control_file {
        match ControlFile::open_or_create_with_piece_length(&ctrl_path, total_length, piece_length)
            .await
        {
            Ok(control_file) => Some(control_file),
            Err(error) => {
                tracing::warn!(
                    "Failed to create control file {}: {}. Resume will be less reliable.",
                    ctrl_path.display(),
                    error
                );
                None
            }
        }
    } else {
        None
    };

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
    let completed_bytes = if let Some(control_file) = ctrl_file.as_ref() {
        if compatible_control_file.is_some() && control_file.completed_pieces() > 0 {
            manager.restore_completed_from_bitfield(control_file.bitfield())
        } else if compatible_control_file.is_some() || can_restore_prefix {
            manager.restore_completed_prefix(persisted_prefix)
        } else {
            0
        }
    } else if can_restore_prefix {
        manager.restore_completed_prefix(resume_state.start_offset)
    } else {
        0
    };
    dl.progress_updater.reset(completed_bytes);
    dl.progress.set_completed_length(completed_bytes);
    if resume_state.should_resume {
        tracing::debug!(
            existing_length = resume_state.existing_length,
            start_offset = resume_state.start_offset,
            restored_bytes = completed_bytes,
            "Resuming single-source download from persisted segment state"
        );
    }

    if let Some(control_file) = ctrl_file.as_mut() {
        let payload_ready =
            if completed_bytes > 0 && completed_bytes != control_file.completed_length() {
                match writer.sync_all().await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::debug!(
                            completed_bytes,
                            %error,
                            "Not persisting an initial checkpoint for an unsynced resume prefix"
                        );
                        false
                    }
                }
            } else {
                true
            };
        if payload_ready {
            control_file.update_completed_length(completed_bytes);
            if let Err(error) = control_file.save().await {
                tracing::warn!("Failed to save initial control file: {}", error);
            }
        }
    }
    let ctrl_save_interval = total_length / num_pieces as u64;
    flush_requested_control_file(dl, writer, &mut ctrl_file, completed_bytes).await?;

    Ok(PreparedResume {
        ctrl_path,
        ctrl_file,
        ctrl_save_interval,
        completed_bytes,
    })
}
