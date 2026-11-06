use std::time::Instant;

use crate::engine::bittorrent::download::command::BtDownloadCommand;

use super::PieceDownloadSession;

impl PieceDownloadSession<'_> {
    pub(super) fn refresh_download_progress(&mut self) {
        self.command
            .progress
            .set_completed_length(self.command.completed_bytes);
        self.refresh_upload_stats();
    }

    pub(super) fn refresh_upload_stats(&mut self) {
        let uploaded_by_peers = self
            .upload_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        let delta = uploaded_by_peers.saturating_sub(self.last_uploaded);
        if delta > 0 {
            self.command.total_uploaded = self.command.total_uploaded.saturating_add(delta);
            self.command
                .progress
                .set_upload_length(self.command.total_uploaded);
            self.last_uploaded = uploaded_by_peers;
        }

        self.command
            .progress
            .set_upload_speed(self.swarm.upload_speed_at(Instant::now()));
    }
}

impl BtDownloadCommand {
    /// Periodically save download progress to .aria2 file.
    /// Called after a piece is successfully verified and written.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn maybe_save_progress(
        &self,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        bitfield: &std::sync::Arc<std::sync::RwLock<Vec<u8>>>,
        piece_length: u32,
        total_size: u64,
        num_pieces: u32,
        start_time: Instant,
        last_progress_save: &mut Instant,
        next_piece_idx: usize,
    ) {
        if let Some(ref mgr) = self.progress_manager
            && last_progress_save.elapsed() >= self.progress_save_interval
        {
            let bitfield = super::super::super::checkpoint::snapshot_completed_bitfield(bitfield);
            let progress = super::super::progress_snapshot(
                meta.network_info_hash(),
                &bitfield,
                piece_length,
                total_size,
                num_pieces,
                crate::engine::bittorrent::persistence::progress_info_file::DownloadStats {
                    downloaded_bytes: self.completed_bytes,
                    uploaded_bytes: self.total_uploaded,
                    upload_speed: 0.0,
                    download_speed: 0.0,
                    elapsed_seconds: start_time.elapsed().as_secs(),
                },
            );

            match mgr.save_progress(&meta.network_info_hash(), &progress) {
                Ok(()) => {
                    *last_progress_save = Instant::now();
                    tracing::debug!(
                        pieces_completed = next_piece_idx + 1,
                        total_pieces = num_pieces,
                        "BT progress saved successfully"
                    );
                }
                Err(e) => tracing::warn!(error = %e, "Failed to save BT progress"),
            }
        }
    }
}
