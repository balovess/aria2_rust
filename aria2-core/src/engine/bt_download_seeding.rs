use tracing::info;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
use crate::engine::bt_upload_session::BtSeedingConfig;
use crate::error::Result;

impl BtDownloadCommand {
    pub async fn run_seeding_phase(
        &mut self,
        connections: Vec<BtPeerConn>,
        piece_length: u32,
        num_pieces: u32,
    ) -> Result<()> {
        if connections.is_empty() {
            info!("No active peers for seeding");
            return Ok(());
        }

        let file_provider = std::sync::Arc::new(FileBackedPieceProvider::new(
            self.output_path.clone(),
            piece_length,
            num_pieces,
            self.multi_file_layout.clone(),
        ));

        let upload_limit = { self.group.read().await.options().max_upload_limit };
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
                .filter_map(|c| match c {
                    BtPeerConn::Plain(p) => Some(p),
                    _ => None,
                })
                .collect();

        let mut manager = BtSeedManager::new_with_choking_algo(
            plain_connections,
            file_provider,
            config,
            exit_cond,
            self.completed_bytes,
            self.choking_algo.take(),
        );
        manager.run_seeding_loop().await?;

        self.total_uploaded = manager.total_uploaded();
        info!(
            "Seeding complete: uploaded {} bytes in {:?}",
            self.total_uploaded,
            manager.seeding_duration()
        );
        Ok(())
    }
}
