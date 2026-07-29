use tracing::{debug, info};

use crate::error::Result;

use super::BtDownloadCommand;

/// Seeding statistics for a completed download.
#[derive(Debug, Clone)]
pub struct SeedStats {
    /// Total bytes uploaded during seeding
    pub total_uploaded: u64,
    /// Current upload speed in bytes/sec
    pub upload_speed: u64,
    /// Upload/download ratio
    pub ratio: f64,
    /// Time elapsed since seeding started
    pub elapsed: std::time::Duration,
}

/// Seeding mode (Phase 16) API.
pub trait BtDownloadCommandSeedApi {
    /// Check if download is complete and start seeding if enabled.
    fn check_and_start_seeding(
        &mut self,
        piece_picker: &aria2_protocol::bittorrent::piece::picker::PiecePicker,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>,
    ) -> Result<bool>;

    fn get_seed_manager(&self) -> Option<&super::super::bt_seed_manager::BtSeedManager>;
    fn get_seed_manager_mut(&mut self) -> Option<&mut super::super::bt_seed_manager::BtSeedManager>;
    fn is_seeding(&self) -> bool;
    fn get_seed_stats(&self) -> Option<SeedStats>;
}

impl BtDownloadCommandSeedApi for BtDownloadCommand {
    fn check_and_start_seeding(
        &mut self,
        piece_picker: &aria2_protocol::bittorrent::piece::picker::PiecePicker,
        meta: &aria2_protocol::bittorrent::torrent::parser::TorrentMeta,
        connections: Vec<aria2_protocol::bittorrent::peer::connection::PeerConnection>,
        piece_provider: std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider>,
    ) -> Result<bool> {
        // Check if download is complete
        if !piece_picker.is_complete() {
            debug!("Download not complete, skipping seeding");
            return Ok(false);
        }

        // Check if seeding is enabled
        if !self.seed_enabled {
            info!("Seeding disabled, not starting seed manager");
            return Ok(false);
        }

        // Initialize seed manager
        use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition};
        use crate::engine::bt_upload_session::BtSeedingConfig;

        let seed_ratio = self.seed_ratio.unwrap_or(0.0);
        let seed_time = self.seed_time.map(|d| d.as_secs());

        let exit_condition =
            SeedExitCondition::with_time_and_ratio(seed_time.unwrap_or(0), seed_ratio);

        let config = BtSeedingConfig {
            max_upload_bytes_per_sec: None, // Will be set from options if needed
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        let total_downloaded = meta.total_size();

        let seed_manager = BtSeedManager::new_with_info_hash(
            meta.info_hash.bytes,
            connections,
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
        );

        self.seed_manager = Some(seed_manager);

        info!(
            "Seeding started: ratio={}, time={:?}, info_hash={}",
            seed_ratio,
            self.seed_time,
            meta.info_hash.as_hex()
        );

        Ok(true)
    }

    fn get_seed_manager(&self) -> Option<&super::super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_ref()
    }

    fn get_seed_manager_mut(&mut self) -> Option<&mut super::super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_mut()
    }

    fn is_seeding(&self) -> bool {
        self.seed_manager.is_some()
    }

    fn get_seed_stats(&self) -> Option<SeedStats> {
        self.seed_manager.as_ref().map(|mgr| {
            let (total_uploaded, upload_speed) = mgr.get_upload_stats();
            let total_downloaded = mgr.total_downloaded();
            let ratio = if total_downloaded > 0 {
                total_uploaded as f64 / total_downloaded as f64
            } else {
                0.0
            };

            SeedStats {
                total_uploaded,
                upload_speed,
                ratio,
                elapsed: mgr.seeding_duration(),
            }
        })
    }
}
