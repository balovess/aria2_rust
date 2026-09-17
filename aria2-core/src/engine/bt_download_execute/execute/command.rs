use async_trait::async_trait;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::command::{Command, CommandStatus};
use crate::error::{Aria2Error, FatalError, Result};
use crate::filesystem::control_file::ControlFile;
#[cfg(feature = "bittorrent")]
use crate::request::request_group::BtConnectionGuard;
use crate::request::request_group::{ActiveConnectionGuard, GroupId};
use crate::util::rwlock_ext::RwLockRecover;

use super::checkpoint::{completed_piece_bytes, legacy_progress_piece_indices};
use super::state::IntegrityPreparation;

#[async_trait]
impl Command for BtDownloadCommand {
    async fn shutdown(&mut self) {
        self.dht_periodic_lookup.cancel_pending_lookup().await;
        BtDownloadCommand::shutdown(self).await;
    }

    async fn execute(&mut self) -> Result<()> {
        let _connection_guard = ActiveConnectionGuard::new(Arc::clone(&self.group));
        #[cfg(feature = "bittorrent")]
        let _bt_connection_guard = BtConnectionGuard::new(Arc::clone(&self.group));
        if !self.started {
            self.group.recover_mut().start()?;
            self.started = true;
            self.started_at = Some(Instant::now());
        }

        self.register_bt_download();

        let (mut meta, piece_length, total_size, num_pieces) = self.prepare_environment().await?;
        let network_info_hash = meta.network_info_hash();
        self.group
            .recover()
            .set_control_file_path(ControlFile::control_path_for(&self.output_path));

        // A zero-length torrent has no pieces or peers to acquire. Complete
        // it after metadata preparation, matching the normal download
        // lifecycle without entering tracker/peer discovery.
        if total_size == 0 {
            self.create_zero_length_payload().await?;
            self.completed_bytes = 0;
            self.progress.set_completed_length(0);
            let payload_exists = self.bt_payload_exists();
            let checkpoint = crate::engine::bt_checkpoint::BtCheckpoint::open(
                &self.output_path,
                payload_exists,
                total_size,
                piece_length,
                num_pieces as usize,
                network_info_hash,
            )
            .await?;
            checkpoint.remove().await?;
            self.group.recover_mut().complete()?;
            info!("BT zero-length download completed without peer discovery");
            return Ok(());
        }

        let IntegrityPreparation {
            payload_exists,
            seed_unverified,
            mut verified_piece_indices,
            integrity_finished_action,
        } = self
            .prepare_integrity_state(
                &meta,
                piece_length,
                total_size,
                num_pieces,
                network_info_hash,
            )
            .await?;

        if self.hash_check_only {
            info!("hash-check-only enabled; stopping after integrity validation");
            if self.check_integrity && verified_piece_indices.len() == num_pieces as usize {
                self.completed_bytes = total_size;
                self.progress.set_completed_length(total_size);
                if let Some(checkpoint) = self.checkpoint.take() {
                    checkpoint.remove().await?;
                }
                self.group.recover_mut().complete()?;
                info!("hash-check-only completed successfully");
                return Ok(());
            }
            return Err(Aria2Error::Fatal(FatalError::Config(
                "hash-check-only: existing data failed torrent piece hash validation".into(),
            )));
        }

        // A successful integrity check with seeding disabled is already a
        // complete download. Finish locally instead of discovering peers or
        // announcing a new torrent session.
        if self.hash_check_completed && !self.bt_hash_check_seed {
            info!(
                "Integrity check completed; bt-hash-check-seed disabled, stopping without seeding"
            );
            let started_at = self.started_at.unwrap_or_else(Instant::now);
            self.finalize_download(started_at, &meta).await?;
            return Ok(());
        }

        // File pre-allocation (mirrors C++ BtFileAllocationEntry queued into
        // FileAllocationMan after integrity checking). Single-file torrents
        // allocate `output_path`; multi-file torrents allocate every file in
        // layout order. Already-completed files are skipped by the worker.
        // The worker runs chunked, cooperative allocation sequentially across
        // downloads, so a huge zero-fill never blocks this task or the engine.
        {
            use crate::filesystem::file_allocation::AllocationStrategy;
            use crate::filesystem::file_allocation_man;
            let strategy = AllocationStrategy::from_str(&self.file_allocation);
            if strategy != AllocationStrategy::None {
                let gid = self.group.recover().gid().value();
                let man = file_allocation_man::shared();
                let allocation_files = if self.hash_check_completed {
                    integrity_finished_action
                        .file_allocation
                        .clone()
                        .unwrap_or_else(|| self.integrity_files(total_size))
                } else {
                    self.integrity_files(total_size)
                };
                if self.multi_file_layout.is_some() {
                    let files: Vec<(std::path::PathBuf, u64)> = allocation_files
                        .into_iter()
                        .map(|file| (file.path, file.length))
                        .collect();
                    file_allocation_man::enqueue_multi(
                        &man,
                        files,
                        strategy,
                        self.secure_falloc,
                        gid,
                    )
                    .await?;
                } else {
                    file_allocation_man::enqueue_path(
                        &man,
                        &self.output_path,
                        total_size,
                        strategy,
                        self.secure_falloc,
                        gid,
                    )
                    .await?;
                }
            }
        }

        // P1 integration: use the C++-compatible progress file only as a
        // fallback when the Rust-owned A2CF has no progress. Integrity checks
        // remain authoritative because a progress file records trust, not
        // fresh hash evidence.
        if let Some(ref mgr) = self.progress_manager {
            match mgr.load_progress(&network_info_hash) {
                Ok(saved)
                    if !self.check_integrity
                        && !seed_unverified
                        && payload_exists
                        && self.completed_bytes == 0 =>
                {
                    match legacy_progress_piece_indices(
                        &saved,
                        piece_length,
                        total_size,
                        num_pieces,
                    ) {
                        Some(indices) if !indices.is_empty() => {
                            self.completed_bytes =
                                completed_piece_bytes(&indices, piece_length, total_size);
                            self.progress.set_completed_length(self.completed_bytes);
                            self.group
                                .recover()
                                .set_bt_bitfield(Some(saved.bitfield.clone()));
                            verified_piece_indices = indices;
                            info!(
                                pieces_done = verified_piece_indices.len(),
                                completed_bytes = self.completed_bytes,
                                "Resuming from legacy BT progress"
                            );
                        }
                        Some(_) => debug!("Saved BT progress has no completed pieces"),
                        None => warn!("Ignoring BT progress with incompatible torrent layout"),
                    }
                }
                Ok(_) => debug!(
                    "Ignoring saved BT progress because a newer checkpoint or integrity result is authoritative"
                ),
                Err(e) => {
                    debug!(
                        error = %e,
                        "No saved progress found, starting fresh download"
                    );
                }
            }
        }

        const PEX_SEND_INTERVAL_SECS: u64 = 60;
        let mut session = self
            .prepare_peer_session(
                &meta,
                piece_length,
                total_size,
                num_pieces,
                network_info_hash,
            )
            .await?;

        let piece_result = self
            .download_pieces_loop(
                &mut session.active_connections,
                &mut meta,
                piece_length,
                total_size,
                num_pieces,
                session.web_seed_manager.as_ref(),
                &mut session.pex_enabled_peers,
                &mut session.last_pex_send,
                PEX_SEND_INTERVAL_SECS,
                &verified_piece_indices,
            )
            .await;
        self.group.recover().clear_bt_peer_snapshots();
        if let Err(error) = piece_result {
            if let Some(ref mut announcer) = self.tracker_announcer {
                announcer
                    .announce_stopped(
                        &network_info_hash,
                        &self.local_peer_id,
                        self.completed_bytes,
                        total_size.saturating_sub(self.completed_bytes),
                        self.total_uploaded,
                    )
                    .await;
            }
            return Err(error);
        }

        if let Some(ref mut announcer) = self.tracker_announcer {
            announcer
                .announce_completed(
                    &network_info_hash,
                    &self.local_peer_id,
                    self.completed_bytes,
                    self.total_uploaded,
                )
                .await;
        }

        if self.seed_enabled {
            info!(
                "Starting seeding phase with {} peers...",
                session.active_connections.len()
            );
            self.run_seeding_phase(
                session.active_connections,
                piece_length,
                num_pieces,
                network_info_hash,
            )
            .await?;
        } else {
            info!("Skipping seeding (enabled={})", self.seed_enabled,);
        }

        let started_at = self.started_at.unwrap_or_else(Instant::now);
        self.finalize_download(started_at, &meta).await?;

        Ok(())
    }

    fn status(&self) -> CommandStatus {
        if self.group.recover().status() == crate::request::request_group::DownloadStatus::Complete
        {
            CommandStatus::Completed
        } else if self.completed_bytes > 0 {
            CommandStatus::Running
        } else {
            CommandStatus::Pending
        }
    }

    fn gid(&self) -> GroupId {
        self.group.recover().gid()
    }

    fn request_group(
        &self,
    ) -> Option<std::sync::Arc<std::sync::RwLock<crate::request::request_group::RequestGroup>>>
    {
        Some(std::sync::Arc::clone(&self.group))
    }

    fn timeout(&self) -> Option<Duration> {
        self.group.recover().timeout()
    }
}
