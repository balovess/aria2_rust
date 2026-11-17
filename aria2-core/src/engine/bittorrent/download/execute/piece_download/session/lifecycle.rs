use crate::engine::work_scheduler::WorkScheduler;
use crate::error::{Aria2Error, Result};
use crate::request::request_group::{DownloadResultCode, HaltReason};
use crate::util::rwlock_ext::RwLockRecover;
use std::time::Instant;

use super::PieceDownloadSession;

pub(super) enum DownloadHalt {
    Continue,
    Timeout,
    Requested,
}

impl PieceDownloadSession<'_> {
    pub(super) async fn run(self) -> Result<()> {
        let upload_speed_reporter =
            crate::engine::bittorrent::download::execute::spawn_upload_speed_reporter(
                std::sync::Arc::clone(&self.command.progress),
                self.swarm.upload_rate(),
            );
        let result = self.run_loop().await;
        upload_speed_reporter.abort();
        let _ = upload_speed_reporter.await;
        result
    }

    pub(super) fn check_download_halt(&mut self) -> DownloadHalt {
        let group = self.command.group.recover();
        if group.is_force_halt_requested() || group.is_halt_requested() {
            return DownloadHalt::Requested;
        }
        let timeout_elapsed = self.stop_timeout.should_halt(
            group.options().bt_stop_timeout,
            self.command.completed_bytes,
            Instant::now(),
        );
        if !timeout_elapsed {
            return DownloadHalt::Continue;
        }

        let timeout_seconds = group.options().bt_stop_timeout.unwrap_or_default();
        tracing::warn!(
            gid = group.gid().value(),
            timeout_seconds,
            "Stopping BitTorrent download after consecutive no-progress timeout"
        );
        group.request_force_halt(HaltReason::Timeout);
        group.set_last_error(DownloadResultCode::TimeOut, "Download timed out");
        DownloadHalt::Timeout
    }

    pub(super) async fn finish_halted_download(
        &mut self,
        piece_work_queue: &mut WorkScheduler<u32>,
        web_seed_work_queue: &mut WorkScheduler<u32>,
        web_seed_tasks: &mut tokio::task::JoinSet<(u32, std::result::Result<Vec<u8>, String>)>,
    ) -> Result<()> {
        piece_work_queue.cancel();
        web_seed_work_queue.cancel();
        web_seed_tasks.abort_all();
        self.sync_checkpoint_payload().await?;
        self.close_halted_writer().await?;
        self.save_checkpoint_snapshot().await?;
        Err(Aria2Error::DownloadFailed(
            "BitTorrent download halted".into(),
        ))
    }

    pub(super) async fn sync_checkpoint_payload(&mut self) -> Result<()> {
        self.command
            .sync_checkpoint_payload(&mut self.writer)
            .await
            .map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to sync halted BT output before checkpoint: {error}"
                ))
            })?;
        Ok(())
    }

    pub(super) async fn save_checkpoint_snapshot(&mut self) -> Result<()> {
        if let Some(checkpoint) = self.command.checkpoint.as_mut() {
            let bitfield = super::super::super::checkpoint::snapshot_completed_bitfield(
                &self.completed_bitfield,
            );
            checkpoint
                .save_with_in_flight_pieces(
                    &bitfield,
                    self.command.completed_bytes,
                    &super::piece_storage::in_flight_snapshot(&self.in_flight_pieces),
                )
                .await
                .map_err(|error| {
                    Aria2Error::FileIo(format!("Failed to save halted BT checkpoint: {error}"))
                })?;
            self.command
                .group
                .recover()
                .take_save_control_file_request();
        }
        Ok(())
    }

    pub(super) async fn close_halted_writer(&mut self) -> Result<()> {
        self.writer.close().await.map_err(|error| {
            Aria2Error::FileIo(format!("Failed to close halted BT output: {error}"))
        })
    }

    pub(super) async fn finalize_completed_download(&mut self) -> Result<()> {
        tracing::info!("[BT] Finalizing writer...");
        self.command
            .sync_dirty_multi_file_payload()
            .await
            .map_err(|error| {
                Aria2Error::FileIo(format!(
                    "Failed to sync remaining BitTorrent files before completion: {error}"
                ))
            })?;
        self.writer
            .flush()
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to flush BT output: {error}")))?;
        self.writer
            .close()
            .await
            .map_err(|error| Aria2Error::FileIo(format!("Failed to close BT output: {error}")))?;
        if let Some(checkpoint) = self.command.checkpoint.take() {
            checkpoint.remove().await?;
        }
        tracing::info!("[BT] Writer flushed and closed OK");
        tracing::info!(
            "BT download done: {} ({} bytes)",
            self.command.output_path.display(),
            self.command.completed_bytes
        );
        Ok(())
    }
}
