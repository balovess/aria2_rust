use tracing::info;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use crate::engine::bt_upload_session::BtSeedingConfig;
use crate::error::Result;
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

        let plain_connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection> =
            connections
                .into_iter()
                .filter_map(|c| match c.inner {
                    crate::engine::bt_peer_connection::InnerConnection::Plain(p) => Some(p),
                    _ => None,
                })
                .collect();

        // Reuse the download announcer so the completed event and tracker
        // timing state remain part of one lifecycle.
        let announcer = self.tracker_announcer.take();
        let peer_id = self.local_peer_id;

        let mut manager = BtSeedManager::new_with_announcer(
            info_hash,
            plain_connections,
            file_provider,
            config,
            exit_cond,
            self.completed_bytes,
            self.choking_algo.take(),
            announcer,
            peer_id,
        );
        let seeding_result = manager.run_seeding_loop().await;
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
}
