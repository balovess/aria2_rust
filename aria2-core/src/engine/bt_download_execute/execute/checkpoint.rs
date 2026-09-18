use std::time::{Duration, Instant};

use tracing::warn;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_progress_info_file::BtProgress;
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;

pub(super) fn checkpoint_save_due(
    save_requested: bool,
    bytes_since_save: u64,
    last_save: Instant,
    now: Instant,
) -> bool {
    save_requested
        || bytes_since_save >= crate::constants::BT_CHECKPOINT_SAVE_BYTES
        || now.saturating_duration_since(last_save)
            >= Duration::from_secs(crate::constants::BT_CHECKPOINT_SAVE_INTERVAL_SECS)
}

pub(super) fn snapshot_completed_bitfield(
    bitfield: &std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
) -> Vec<u8> {
    bitfield
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

pub(super) fn legacy_progress_piece_indices(
    progress: &BtProgress,
    piece_length: u32,
    total_size: u64,
    num_pieces: u32,
) -> Option<Vec<usize>> {
    let num_pieces_usize = num_pieces as usize;
    if !progress.is_torrent
        || progress.piece_length != piece_length
        || progress.total_size != total_size
        || progress.num_pieces != num_pieces
        || progress.bitfield.len() != num_pieces_usize.div_ceil(8)
    {
        return None;
    }

    let unused_bits = (8 - num_pieces_usize % 8) % 8;
    if unused_bits != 0
        && progress
            .bitfield
            .last()
            .is_none_or(|byte| byte & ((1u8 << unused_bits) - 1) != 0)
    {
        return None;
    }

    Some(
        (0..num_pieces_usize)
            .filter(|&index| {
                progress
                    .bitfield
                    .get(index / 8)
                    .is_some_and(|byte| byte & (1 << (7 - index % 8)) != 0)
            })
            .collect(),
    )
}

pub(super) fn completed_piece_bytes(indices: &[usize], piece_length: u32, total_size: u64) -> u64 {
    indices
        .iter()
        .map(|&index| {
            total_size
                .saturating_sub(index as u64 * piece_length as u64)
                .min(piece_length as u64)
        })
        .sum()
}

pub(super) fn initial_bt_progress(
    check_integrity: bool,
    checkpoint_completed_length: u64,
) -> (u64, u64) {
    let command_completed_length = if check_integrity {
        0
    } else {
        checkpoint_completed_length
    };
    (command_completed_length, checkpoint_completed_length)
}

impl BtDownloadCommand {
    pub(super) async fn persist_checkpoint_after_piece(
        &mut self,
        writer: &mut Box<dyn crate::filesystem::disk_writer::SeekableDiskWriter>,
        bitfield: &std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
        piece_bytes: u64,
    ) -> Result<()> {
        let save_requested = self.group.recover().is_save_control_file_requested();
        let Some(checkpoint) = self.checkpoint.as_mut() else {
            if save_requested {
                return Err(Aria2Error::FileIo(
                    "Requested BitTorrent checkpoint is unavailable".into(),
                ));
            }
            return Ok(());
        };

        self.checkpoint_bytes_since_save =
            self.checkpoint_bytes_since_save.saturating_add(piece_bytes);
        if !checkpoint_save_due(
            save_requested,
            self.checkpoint_bytes_since_save,
            self.checkpoint_last_save,
            Instant::now(),
        ) {
            return Ok(());
        }
        let bitfield_snapshot = snapshot_completed_bitfield(bitfield);

        // The single-file BT writer uses a write-back cache. Persist payload
        // bytes before its bitfield so a restored checkpoint never advertises
        // a verified piece whose data is still only in memory.
        writer.flush().await.map_err(|error| {
            Aria2Error::FileIo(format!(
                "Failed to flush BitTorrent checkpoint payload: {error}"
            ))
        })?;

        let save_started = std::time::Instant::now();
        match checkpoint
            .save(&bitfield_snapshot, self.completed_bytes)
            .await
        {
            Ok(()) => {
                self.checkpoint_bytes_since_save = 0;
                self.checkpoint_last_save = std::time::Instant::now();
                tracing::debug!(
                    piece_bytes,
                    save_ms = save_started.elapsed().as_millis() as u64,
                    forced = save_requested,
                    "BT checkpoint persisted"
                );
                if save_requested {
                    self.group.recover().take_save_control_file_request();
                }
                Ok(())
            }
            Err(error) if save_requested => Err(Aria2Error::FileIo(format!(
                "Failed to save requested BitTorrent checkpoint: {error}"
            ))),
            Err(error) => {
                warn!(%error, "Failed to save BT checkpoint after piece completion");
                Ok(())
            }
        }
    }
}
