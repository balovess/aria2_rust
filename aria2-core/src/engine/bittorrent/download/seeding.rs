use tracing::info;

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::download::seed_manager::{
    BtSeedManager, SeedExitCondition, SeedPeerDiscovery,
};
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::interaction::BtPeerConnectionOptions;
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::engine::bittorrent::peer::upload_session::BtSeedingConfig;
use crate::engine::bittorrent::piece::downloader::FileBackedPieceProvider;
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
            std::time::Instant::now(),
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
        last_pex_send: std::time::Instant,
        piece_length: u32,
        num_pieces: u32,
        info_hash: [u8; 20],
        info_hash_v2: Option<[u8; 32]>,
        total_size: u64,
    ) -> Result<()> {
        swarm.set_local_metadata(std::sync::Arc::clone(&self.local_metadata));
        let file_provider: std::sync::Arc<
            dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider,
        > = std::sync::Arc::new(FileBackedPieceProvider::new(
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
            max_peers_to_unchoke: group_options
                .bt_max_upload_slots
                .unwrap_or(crate::constants::BT_DEFAULT_MAX_UPLOAD_SLOTS as u32)
                as usize,
            optimistic_unchoke_interval_secs: group_options
                .bt_optimistic_unchoke_interval
                .unwrap_or(crate::constants::BT_OPTIMISTIC_UNCHOKE_INTERVAL_SECS),
        };

        swarm.set_local_seeder(true);

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
        swarm.set_wanted_pieces(std::sync::Arc::from([]));

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
        let tracker_actor = self.tracker_actor.clone();
        let peer_id = self.local_peer_id;
        let mut connection_options =
            BtPeerConnectionOptions::from_download_options(&group_options, peer_id);
        connection_options.dht_enabled =
            (group_options.enable_dht || group_options.enable_dht6) && !self.is_private;
        connection_options.listen_port = (self.listen_port != 0).then_some(self.listen_port);
        connection_options.hybrid_info_hash_v2 = info_hash_v2;
        let discovery = SeedPeerDiscovery {
            group: std::sync::Arc::clone(&self.group),
            dht_engines: if self.is_private {
                crate::engine::bittorrent::dht::engine_set::DhtEngineSet::default()
            } else {
                self.dht_engines.clone()
            },
            dht_lookup: std::mem::take(&mut self.dht_periodic_lookup),
            listen_port: self.listen_port,
            connection_options,
            total_size,
            utp_socket: self.utp_socket.clone(),
            outbound_network_policy: std::sync::Arc::clone(&self.outbound_network_policy),
            enable_peer_exchange: group_options.enable_peer_exchange && !self.is_private,
        };

        let upload_rate = swarm.upload_rate();
        let mut manager = BtSeedManager::new_with_swarm(
            info_hash,
            swarm,
            file_provider,
            config,
            exit_cond,
            self.completed_bytes,
            tracker_actor,
            peer_id,
            self.incoming_peers.take(),
            upload_counter,
            last_pex_send,
        )
        .with_torrent_upload_limiter(self.torrent_upload_limiter.clone())
        .with_peer_storage(std::sync::Arc::clone(&self.peer_storage))
        .with_peer_discovery(discovery);
        self.attach_seed_observers(&mut manager);
        let upload_speed_reporter =
            crate::engine::bittorrent::download::execute::spawn_upload_speed_reporter(
                std::sync::Arc::clone(&self.progress),
                upload_rate,
            );
        let lifecycle_notifier = self.group.recover().lifecycle_notifier();
        let cancellation_token = manager.cancellation_token();
        let mut seeding_loop = Box::pin(manager.run_seeding_loop());
        let mut lifecycle_error = None;
        let seeding_result = loop {
            if lifecycle_error.is_none()
                && let Some(error) = self.seeding_lifecycle_error()
            {
                lifecycle_error = Some(error);
                cancellation_token.cancel();
            }

            let lifecycle_changed = lifecycle_notifier.notified();
            tokio::pin!(lifecycle_changed);
            tokio::select! {
                result = &mut seeding_loop => {
                    if let Some(error) = lifecycle_error.take() {
                        break Err(error);
                    }
                    if let Some(error) = self.seeding_lifecycle_error() {
                        break Err(error);
                    }
                    break result;
                }
                _ = &mut lifecycle_changed => {
                    if lifecycle_error.is_none()
                        && let Some(error) = self.seeding_lifecycle_error()
                    {
                        lifecycle_error = Some(error);
                        cancellation_token.cancel();
                    }
                }
            }
        };
        drop(seeding_loop);
        upload_speed_reporter.abort();
        let _ = upload_speed_reporter.await;
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
