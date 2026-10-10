use std::time::{Duration, Instant};

use tracing::warn;

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::persistence::progress_info_file::BtProgress;
use crate::error::{Aria2Error, Result};
use crate::filesystem::disk_writer::SeekableDiskWriter;
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

pub(super) fn mark_piece_completed(
    bitfield: &std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
    piece_index: u32,
) {
    let mut bitfield = bitfield
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(byte) = bitfield.get_mut(piece_index as usize / 8) {
        *byte |= 1 << (7 - piece_index % 8);
    }
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
    pub(super) async fn write_multi_file_piece_and_track(
        &mut self,
        piece_index: u32,
        piece_data: &bytes::Bytes,
        max_open_files: usize,
    ) -> Result<()> {
        let layout = self
            .multi_file_layout
            .as_ref()
            .filter(|layout| layout.is_multi_file())
            .ok_or_else(|| {
                Aria2Error::FileIo(
                    "Multi-file piece write requested without a multi-file layout".into(),
                )
            })?;
        let touched_files = crate::engine::bittorrent::piece::downloader::write_piece_to_multi_files_coalesced_with_limit_tracked(
            layout,
            piece_index,
            piece_data,
            layout.piece_length(),
            max_open_files,
        )
        .await?;
        self.dirty_multi_file_indices.extend(touched_files);
        Ok(())
    }

    pub(super) async fn sync_checkpoint_payload(
        &mut self,
        writer: &mut Box<dyn crate::filesystem::disk_writer::SeekableDiskWriter>,
    ) -> Result<()> {
        if self
            .multi_file_layout
            .as_ref()
            .is_some_and(|layout| layout.is_multi_file())
        {
            self.sync_dirty_multi_file_payload().await
        } else {
            writer.sync_data().await.map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to durably sync BitTorrent checkpoint payload: {error}"
                ))
            })
        }
    }

    pub(super) async fn sync_dirty_multi_file_payload(&mut self) -> Result<()> {
        let Some(layout) = self
            .multi_file_layout
            .as_ref()
            .filter(|layout| layout.is_multi_file())
        else {
            return Ok(());
        };

        let mut file_indices = self
            .dirty_multi_file_indices
            .iter()
            .copied()
            .collect::<Vec<_>>();
        file_indices.sort_unstable();
        for file_index in file_indices {
            let file_path = layout
                .file_absolute_path(file_index)
                .ok_or_else(|| {
                    Aria2Error::FileIo(format!("Invalid dirty BitTorrent file index {file_index}"))
                })?
                .to_path_buf();
            let mut file_writer =
                crate::filesystem::positioned_disk_writer::PositionedDiskWriter::new(
                    &file_path, None,
                );
            file_writer.open().await.map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to open dirty BitTorrent file {} for sync: {error}",
                    file_path.display()
                ))
            })?;
            let sync_result = file_writer.sync_data().await;
            let close_result = file_writer
                .close_without_sync("BT checkpoint file close")
                .await;
            sync_result.map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to durably sync dirty BitTorrent file {}: {error}",
                    file_path.display()
                ))
            })?;
            close_result.map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to close dirty BitTorrent file {} after sync: {error}",
                    file_path.display()
                ))
            })?;
        }
        self.dirty_multi_file_indices.clear();
        Ok(())
    }

    pub(super) async fn persist_checkpoint_after_piece(
        &mut self,
        writer: &mut Box<dyn crate::filesystem::disk_writer::SeekableDiskWriter>,
        bitfield: &std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
        piece_bytes: u64,
        in_flight_pieces: &[crate::filesystem::control_file::ControlFileInFlightPiece],
        payload_already_synced: bool,
    ) -> Result<()> {
        let save_requested = self.group.recover().is_save_control_file_requested();
        if self.checkpoint.is_none() {
            if save_requested {
                return Err(Aria2Error::FileIo(
                    "Requested BitTorrent checkpoint is unavailable".into(),
                ));
            }
            return Ok(());
        }

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

        // Persist payload bytes before the bitfield so a restored checkpoint
        // never advertises data that is still only in memory or page cache.
        // Piece commits sync before updating live completion state; other
        // checkpoint callers pass `false` and establish the barrier here.
        if !payload_already_synced {
            self.sync_checkpoint_payload(writer)
                .await
                .map_err(|error| {
                    Aria2Error::FileIo(format!(
                        "Failed to sync BitTorrent checkpoint payload: {error}"
                    ))
                })?;
        }

        let save_started = std::time::Instant::now();
        let checkpoint = self
            .checkpoint
            .as_mut()
            .expect("checkpoint presence was checked before syncing payload");
        match checkpoint
            .save_with_in_flight_pieces(&bitfield_snapshot, self.completed_bytes, in_flight_pieces)
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use async_trait::async_trait;

    use crate::engine::bittorrent::download::command::BtDownloadCommand;
    use crate::engine::bittorrent::persistence::checkpoint::BtCheckpoint;
    use crate::error::Result;
    use crate::filesystem::disk_writer::SeekableDiskWriter;
    use crate::filesystem::positioned_disk_writer::PositionedDiskWriter;
    use crate::request::request_group::{DownloadOptions, GroupId};
    use aria2_protocol::bittorrent::torrent::parser::{FileEntry, InfoDict};

    struct SyncTrackingWriter {
        inner: PositionedDiskWriter,
        checkpoint_path: PathBuf,
        sync_calls: Arc<AtomicUsize>,
        sync_preceded_checkpoint: Arc<AtomicBool>,
        fail_sync: bool,
    }

    #[async_trait]
    impl SeekableDiskWriter for SyncTrackingWriter {
        async fn open(&mut self) -> Result<()> {
            self.inner.open().await
        }

        async fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
            self.inner.write_at(offset, data).await
        }

        async fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.inner.read_at(offset, buf).await
        }

        async fn truncate(&mut self, length: u64) -> Result<()> {
            self.inner.truncate(length).await
        }

        async fn flush(&mut self) -> Result<()> {
            self.inner.flush().await
        }

        async fn sync_data(&mut self) -> Result<()> {
            self.sync_calls.fetch_add(1, Ordering::SeqCst);
            self.sync_preceded_checkpoint
                .store(!self.checkpoint_path.exists(), Ordering::SeqCst);
            if self.fail_sync {
                return Err(crate::error::Aria2Error::Io(
                    "injected payload sync failure".into(),
                ));
            }
            self.inner.sync_data().await
        }

        async fn len(&self) -> Result<u64> {
            self.inner.len().await
        }

        fn path(&self) -> &Path {
            self.inner.path()
        }

        async fn close(&mut self) -> Result<()> {
            self.inner.close().await
        }
    }

    async fn command_with_checkpoint(
        output_dir: &Path,
        gid: u64,
    ) -> (BtDownloadCommand, PathBuf, u32) {
        let torrent = crate::engine::bittorrent::download::command_tests::build_test_torrent();
        let options = DownloadOptions::default();
        let output_dir = output_dir.to_string_lossy();
        let mut command = BtDownloadCommand::new(
            GroupId::new(gid),
            &torrent,
            &options,
            Some(output_dir.as_ref()),
        )
        .expect("test BT command should construct");
        let piece_length = crate::constants::BT_CHECKPOINT_SAVE_BYTES as u32;
        command.checkpoint = Some(
            BtCheckpoint::open(
                &command.output_path,
                false,
                u64::from(piece_length),
                piece_length,
                1,
                [0x37; 20],
            )
            .await
            .expect("checkpoint should open"),
        );
        command.completed_bytes = u64::from(piece_length);
        let checkpoint_path =
            crate::filesystem::control_file::ControlFile::control_path_for(&command.output_path);
        (command, checkpoint_path, piece_length)
    }

    fn tracking_writer(
        command: &BtDownloadCommand,
        checkpoint_path: PathBuf,
        sync_calls: &Arc<AtomicUsize>,
        sync_preceded_checkpoint: &Arc<AtomicBool>,
        fail_sync: bool,
    ) -> Box<dyn SeekableDiskWriter> {
        let tracked = SyncTrackingWriter {
            inner: PositionedDiskWriter::new(&command.output_path, None),
            checkpoint_path,
            sync_calls: Arc::clone(sync_calls),
            sync_preceded_checkpoint: Arc::clone(sync_preceded_checkpoint),
            fail_sync,
        };
        Box::new(crate::rate_limiter::ThrottledWriter::new(
            tracked,
            crate::rate_limiter::RateLimiter::unlimited(),
        ))
    }

    #[tokio::test]
    async fn checkpoint_syncs_payload_before_publishing_piece_bitfield() {
        let dir = tempfile::tempdir().expect("temporary BT output directory");
        let (mut command, checkpoint_path, piece_length) =
            command_with_checkpoint(dir.path(), 73_001).await;
        assert!(!checkpoint_path.exists());

        let sync_calls = Arc::new(AtomicUsize::new(0));
        let sync_preceded_checkpoint = Arc::new(AtomicBool::new(false));
        let mut writer = tracking_writer(
            &command,
            checkpoint_path.clone(),
            &sync_calls,
            &sync_preceded_checkpoint,
            false,
        );

        command
            .persist_checkpoint_after_piece(
                &mut writer,
                &Arc::new(std::sync::RwLock::new(vec![0x80])),
                u64::from(piece_length),
                &[],
                false,
            )
            .await
            .expect("payload sync and checkpoint should succeed");

        assert_eq!(sync_calls.load(Ordering::SeqCst), 1);
        assert!(sync_preceded_checkpoint.load(Ordering::SeqCst));
        assert!(checkpoint_path.is_file());
    }

    #[tokio::test]
    async fn checkpoint_is_not_published_when_payload_sync_fails() {
        let dir = tempfile::tempdir().expect("temporary BT output directory");
        let (mut command, checkpoint_path, piece_length) =
            command_with_checkpoint(dir.path(), 73_002).await;
        let sync_calls = Arc::new(AtomicUsize::new(0));
        let sync_preceded_checkpoint = Arc::new(AtomicBool::new(false));
        let mut writer = tracking_writer(
            &command,
            checkpoint_path.clone(),
            &sync_calls,
            &sync_preceded_checkpoint,
            true,
        );

        let result = command
            .persist_checkpoint_after_piece(
                &mut writer,
                &Arc::new(std::sync::RwLock::new(vec![0x80])),
                u64::from(piece_length),
                &[],
                false,
            )
            .await;

        assert!(result.is_err());
        assert_eq!(sync_calls.load(Ordering::SeqCst), 1);
        assert!(sync_preceded_checkpoint.load(Ordering::SeqCst));
        assert!(!checkpoint_path.exists());
    }

    #[tokio::test]
    async fn multi_file_checkpoint_syncs_only_modified_payload_files() {
        let dir = tempfile::tempdir().expect("temporary BT output directory");
        let (mut command, _, _) = command_with_checkpoint(dir.path(), 73_003).await;
        let base_dir = dir.path().join("multi");
        let info = InfoDict {
            name: "multi".into(),
            piece_length: 4,
            pieces: vec![[0; 20], [1; 20]],
            length: None,
            files: Some(vec![
                FileEntry {
                    length: 4,
                    path: vec!["first.bin".into()],
                },
                FileEntry {
                    length: 4,
                    path: vec!["untouched.bin".into()],
                },
            ]),
            private: None,
            meta_version: None,
            v2_files: None,
            pieces_root: None,
        };
        let layout =
            crate::engine::bittorrent::torrent::file_layout::MultiFileLayout::from_info_dict(
                &info, &base_dir,
            )
            .expect("multi-file layout should build");
        layout
            .create_directories()
            .expect("payload directories should be created");
        let modified_file = layout.file_absolute_path(0).unwrap().to_path_buf();
        let untouched_file = layout.file_absolute_path(1).unwrap().to_path_buf();
        std::fs::write(&modified_file, b"data").expect("modified payload should exist");

        command.multi_file_layout = Some(layout);
        command.dirty_multi_file_indices.insert(0);
        command
            .sync_dirty_multi_file_payload()
            .await
            .expect("modified payload should sync");

        assert!(command.dirty_multi_file_indices.is_empty());
        assert_eq!(std::fs::read(modified_file).unwrap(), b"data");
        assert!(
            !untouched_file.exists(),
            "clean torrent files must not be opened or created during checkpoint sync"
        );
    }

    #[tokio::test]
    async fn verified_multi_file_piece_write_registers_each_touched_file() {
        let dir = tempfile::tempdir().expect("temporary BT output directory");
        let (mut command, _, _) = command_with_checkpoint(dir.path(), 73_004).await;
        let base_dir = dir.path().join("multi-write");
        let info = InfoDict {
            name: "multi-write".into(),
            piece_length: 8,
            pieces: vec![[0; 20]],
            length: None,
            files: Some(vec![
                FileEntry {
                    length: 4,
                    path: vec!["first.bin".into()],
                },
                FileEntry {
                    length: 4,
                    path: vec!["second.bin".into()],
                },
            ]),
            private: None,
            meta_version: None,
            v2_files: None,
            pieces_root: None,
        };
        let layout =
            crate::engine::bittorrent::torrent::file_layout::MultiFileLayout::from_info_dict(
                &info, &base_dir,
            )
            .expect("multi-file layout should build");
        layout
            .create_directories()
            .expect("payload directories should be created");
        let first_file = layout.file_absolute_path(0).unwrap().to_path_buf();
        let second_file = layout.file_absolute_path(1).unwrap().to_path_buf();
        command.multi_file_layout = Some(layout);

        command
            .write_multi_file_piece_and_track(0, &bytes::Bytes::from_static(b"ABCDEFGH"), 2)
            .await
            .expect("piece should be written and tracked");

        assert_eq!(
            command.dirty_multi_file_indices,
            std::collections::HashSet::from([0, 1]),
            "a cross-file piece must register both payload paths for checkpoint sync"
        );
        assert_eq!(std::fs::read(first_file).unwrap(), b"ABCD");
        assert_eq!(std::fs::read(second_file).unwrap(), b"EFGH");
    }
}
