use tracing::{debug, info};

use crate::error::Result;
use crate::util::rwlock_ext::RwLockRecover;

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

impl BtDownloadCommand {
    /// Check if download is complete and start seeding if enabled.
    ///
    /// This method should be called after the download loop completes.
    /// It initializes the seed manager if:
    /// - All pieces are complete
    /// - Seeding is enabled (seed_ratio > 0 or seed_time > 0)
    ///
    /// Returns Ok(true) if seeding was started, Ok(false) if not.
    pub fn check_and_start_seeding(
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
            max_upload_bytes_per_sec: self.group.recover().options().max_upload_limit,
            global_limiter: self.global_limiter.clone(),
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        let total_downloaded = meta.total_size();

        let caretaker_id = self.group.recover().gid().value();
        {
            let mut storage = self
                .peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for connection in &connections {
                let Some(endpoint) = connection.remote_addr() else {
                    continue;
                };
                let entry = crate::engine::bt_peer_storage::PeerEntry::new(
                    endpoint.ip().to_string(),
                    endpoint.port(),
                );
                if storage.add_and_checkout_peer(entry, caretaker_id).is_some() {
                    storage.set_peer_active(&endpoint.ip().to_string(), endpoint.port(), true);
                }
            }
        }

        let seed_manager = BtSeedManager::new_with_info_hash(
            meta.info_hash.bytes,
            connections,
            piece_provider,
            config,
            exit_condition,
            total_downloaded,
        )
        .with_peer_storage(std::sync::Arc::clone(&self.peer_storage));

        self.seed_manager = Some(seed_manager);

        info!(
            "Seeding started: ratio={}, time={:?}, info_hash={}",
            seed_ratio,
            self.seed_time,
            meta.info_hash.as_hex()
        );

        Ok(true)
    }

    /// Get a reference to the seed manager.
    pub fn get_seed_manager(&self) -> Option<&super::super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_ref()
    }

    /// Get a mutable reference to the seed manager.
    pub fn get_seed_manager_mut(
        &mut self,
    ) -> Option<&mut super::super::bt_seed_manager::BtSeedManager> {
        self.seed_manager.as_mut()
    }

    /// Check if seeding is active.
    pub fn is_seeding(&self) -> bool {
        self.seed_manager.is_some()
    }

    /// Get seeding statistics.
    ///
    /// Returns None if not seeding.
    pub fn get_seed_stats(&self) -> Option<SeedStats> {
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
