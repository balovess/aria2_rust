use tracing::info;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_message_handler::PeerSwarm;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::BtPeerConnectionOptions;
use crate::engine::bt_piece_downloader::FileBackedPieceProvider;
use crate::engine::bt_seed_manager::{BtSeedManager, SeedExitCondition, SeedPeerDiscovery};
use crate::engine::bt_upload_session::BtSeedingConfig;
use crate::error::{Aria2Error, Result};
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub(crate) fn attach_seed_observers(&self, manager: &mut BtSeedManager) {
        let (connection_state, peer_snapshot_store) = {
            let group = self.group.recover();
            (group.connection_state(), group.bt_peer_snapshot_store())
        };
        manager.set_connection_state(connection_state, peer_snapshot_store);
        manager.set_total_uploaded(self.total_uploaded);
        manager.set_upload_progress(std::sync::Arc::clone(&self.progress));
    }

    pub async fn run_seeding_phase(
        &mut self,
        connections: Vec<BtPeerConn>,
        piece_length: u32,
        num_pieces: u32,
        info_hash: [u8; 20],
    ) -> Result<()> {
        let upload_counter =
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(self.total_uploaded));
        self.run_seeding_phase_with_swarm(
            connections,
            PeerSwarm::new(64),
            upload_counter,
            piece_length,
            num_pieces,
            info_hash,
            None,
            self.completed_bytes,
        )
        .await
    }

    // Keep the lifecycle handoff resources explicit instead of wrapping them in a one-use struct.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_seeding_phase_with_swarm(
        &mut self,
        connections: Vec<BtPeerConn>,
        mut swarm: PeerSwarm,
        upload_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
        piece_length: u32,
        num_pieces: u32,
        info_hash: [u8; 20],
        info_hash_v2: Option<[u8; 32]>,
        total_size: u64,
    ) -> Result<()> {
        let file_provider: std::sync::Arc<dyn crate::engine::bt_upload_session::PieceDataProvider> =
            std::sync::Arc::new(FileBackedPieceProvider::new(
                self.output_path.clone(),
                piece_length,
                num_pieces,
                self.multi_file_layout.clone(),
            ));

        let group_options = { self.group.recover().options().clone() };
        let upload_limit = group_options.max_upload_limit;
        let config = BtSeedingConfig {
            max_upload_bytes_per_sec: upload_limit,
            global_limiter: self.global_limiter.clone(),
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };

        // Promote the completed download's still-connected peers into the
        // TorrentSession-owned registry before transferring that same
        // registry to the seeding coordinator.
        for mut connection in connections {
            connection.configure_upload_with_auto_unchoke(
                &config,
                self.torrent_upload_limiter.clone(),
                num_pieces,
                piece_length,
                false,
            );
            connection.stats.am_choking = true;
            connection.set_upload_counter(std::sync::Arc::clone(&upload_counter));
            if let Err(connection) =
                swarm.spawn_peer(connection, None, std::sync::Arc::clone(&file_provider))
            {
                swarm.shutdown_all().await;
                return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                    format!(
                        "failed to admit completed torrent peer into its swarm: {}:{}",
                        connection.remote_ip(),
                        connection.remote_port()
                    ),
                )));
            }
        }
        swarm.set_wanted_pieces(std::sync::Arc::from([])).await;

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

        // Reuse the download announcer so the completed event and tracker
        // timing state remain part of one lifecycle.
        let announcer = self.tracker_announcer.take();
        let peer_id = self.local_peer_id;
        let mut connection_options =
            BtPeerConnectionOptions::from_download_options(&group_options, peer_id);
        connection_options.dht_enabled = group_options.enable_dht && !self.is_private;
        connection_options.listen_port = (self.listen_port != 0).then_some(self.listen_port);
        connection_options.hybrid_info_hash_v2 = info_hash_v2;
        let discovery = SeedPeerDiscovery {
            group: std::sync::Arc::clone(&self.group),
            dht_engine: if self.is_private {
                None
            } else {
                self.dht_engine.clone()
            },
            dht_lookup: std::mem::take(&mut self.dht_periodic_lookup),
            connection_options,
            total_size,
            utp_socket: self.utp_socket.clone(),
            outbound_network_policy: std::sync::Arc::clone(&self.outbound_network_policy),
            enable_peer_exchange: group_options.enable_peer_exchange && !self.is_private,
        };

        let mut manager = BtSeedManager::new_with_swarm(
            info_hash,
            swarm,
            file_provider,
            config,
            exit_cond,
            self.completed_bytes,
            announcer,
            peer_id,
            self.incoming_peers.take(),
            upload_counter,
        )
        .with_torrent_upload_limiter(self.torrent_upload_limiter.clone())
        .with_peer_storage(std::sync::Arc::clone(&self.peer_storage))
        .with_peer_discovery(discovery);
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
