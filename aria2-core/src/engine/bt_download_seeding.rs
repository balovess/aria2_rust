use tracing::info;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use crate::engine::bt_upload_session::BtSeedingConfig;
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub async fn run_seeding_phase(
        &mut self,
        connections: Vec<BtPeerConn>,
        piece_length: u32,
        num_pieces: u32,
        info_hash: [u8; 20],
    ) -> Result<()> {
        let file_provider = std::sync::Arc::new(FileBackedPieceProvider::new(
            self.output_path.clone(),
            piece_length,
            num_pieces,
            self.multi_file_layout.clone(),
        ));

        let upload_limit = { self.group.recover().options().max_upload_limit };
        let config = BtSeedingConfig {
            max_upload_bytes_per_sec: upload_limit,
            global_limiter: self.global_limiter.clone(),
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        let exit_cond = match (self.seed_time, self.seed_ratio) {
            (Some(t), Some(r)) => SeedExitCondition {
                seed_time: Some(t),
                seed_ratio: Some(r),
            },
            (Some(t), None) => SeedExitCondition {
                seed_time: Some(t),
                seed_ratio: None,
            },
            (None, Some(r)) => SeedExitCondition {
                seed_time: None,
                seed_ratio: Some(r),
            },
            (None, None) => SeedExitCondition::infinite(),
        };

        let upload_connections = connections
            .into_iter()
            .filter_map(|connection| connection.into_upload_connection())
            .collect();

        // Reuse the download announcer so the completed event and tracker
        // timing state remain part of one lifecycle.
        let announcer = self.tracker_announcer.take();
        let peer_id = self.local_peer_id;

        let mut manager = BtSeedManager::new_with_transports(
            info_hash,
            upload_connections,
            file_provider,
            config,
            exit_cond,
            self.completed_bytes,
            self.choking_algo.take(),
            announcer,
            peer_id,
            self.incoming_peers.take(),
        );
        self.attach_seed_observers(&mut manager);
        let lifecycle_notifier = self.group.recover().lifecycle_notifier();
        let seeding_result = loop {
            if let Some(error) = self.seeding_lifecycle_error() {
                manager.cancel();
                if let Err(cleanup_error) = manager.run_seeding_loop().await {
                    tracing::warn!(%cleanup_error, "BitTorrent seeding cleanup failed after lifecycle cancellation");
                }
                break Err(error);
            }

            let lifecycle_changed = lifecycle_notifier.notified();
            tokio::pin!(lifecycle_changed);
            tokio::select! {
                result = manager.run_seeding_loop() => {
                    if let Some(error) = self.seeding_lifecycle_error() {
                        break Err(error);
                    }
                    break result;
                }
                _ = &mut lifecycle_changed => {}
            }
        };
        self.tracker_announcer = manager.take_announcer();
        seeding_result?;

        if manager.halt_requested() {
            info!("Seeding exit criteria reached");
        }
        self.total_uploaded = manager.total_uploaded();
        info!(
            "Seeding complete: uploaded {} bytes in {:?}",
            self.total_uploaded,
            manager.seeding_duration()
        );
        Ok(())
    }

    fn seeding_lifecycle_error(&self) -> Option<Aria2Error> {
        let group = self.group.recover();
        if group.is_removed() {
            Some(Aria2Error::DownloadFailed(
                "Download cancelled by user".into(),
            ))
        } else if group.is_paused_flag() {
            Some(Aria2Error::DownloadFailed("Download paused".into()))
        } else if group.is_force_halt_requested() || group.is_halt_requested() {
            Some(Aria2Error::DownloadFailed("Download halted".into()))
        } else {
            None
        }
    }
}
