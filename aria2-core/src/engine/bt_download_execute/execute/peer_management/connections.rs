use std::collections::HashSet;

use tracing::warn;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_handshake_validation::filter_duplicate_peer_connections;
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_peer_interaction::{BtPeerConnectionOptions, BtPeerInteraction};
use crate::engine::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::peer_stats::PeerStats;
use crate::error::{Aria2Error, RecoverableError, Result};
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    fn return_checked_out_peers(
        &self,
        checked_out: &[(
            aria2_protocol::bittorrent::peer::connection::PeerAddr,
            crate::engine::bt_peer_storage::PeerEntry,
        )],
    ) {
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (_, peer) in checked_out {
            storage.return_peer(peer);
        }
    }

    async fn persist_checkpoint_for_halt(&mut self) -> Result<()> {
        let bitfield = self.group.recover().get_bt_bitfield();
        let Some(checkpoint) = self.checkpoint.as_mut() else {
            return Ok(());
        };
        let bitfield = bitfield
            .or_else(|| checkpoint.bitfield().map(ToOwned::to_owned))
            .unwrap_or_default();
        checkpoint.save(&bitfield, self.completed_bytes).await
    }

    async fn announce_stopped_for_halt(&mut self, info_hash: &[u8; 20], total_size: u64) {
        if let Some(announcer) = self.tracker_announcer.as_mut() {
            announcer
                .announce_stopped(
                    info_hash,
                    &self.local_peer_id,
                    self.completed_bytes,
                    total_size.saturating_sub(self.completed_bytes),
                    self.total_uploaded,
                )
                .await;
        }
    }

    fn reconcile_checked_out_peers(
        &self,
        checked_out: &[(
            aria2_protocol::bittorrent::peer::connection::PeerAddr,
            crate::engine::bt_peer_storage::PeerEntry,
        )],
        active_connections: &[BtPeerConn],
    ) {
        let active: HashSet<_> = active_connections
            .iter()
            .map(|peer| (peer.ip_addr.clone(), peer.port))
            .collect();
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (address, peer) in checked_out {
            if active.contains(&(address.ip.clone(), address.port)) {
                storage.set_peer_active(&peer.ip, peer.port, true);
            } else {
                storage.return_peer(peer);
            }
        }
    }

    pub(in crate::engine::bt_download_execute::execute) fn return_all_checked_out_peers(&self) {
        let mut storage = self
            .peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let peers: Vec<_> = storage.used_peers().iter().cloned().collect();
        for peer in peers {
            storage.return_peer(&peer);
        }
    }

    /// Establish connections to discovered peers and initialize the choking algorithm.
    ///
    /// After establishing connections, this method filters out:
    /// - Self-connections (peer ID matching our own local peer ID)
    /// - Duplicate connections (two connections sharing the same remote peer ID)
    ///
    /// Mirrors C++ `DefaultBtInteractive::receiveHandshake()` which checks:
    /// 1. `memcmp(message->getPeerId(), bittorrent::getStaticPeerId(), 20) == 0`
    ///    → disconnect self-connection
    /// 2. `for(auto& peer : peerStorage_->getUsedPeers()) { memcmp(...) }`
    ///    → disconnect duplicate peer
    pub(in crate::engine::bt_download_execute::execute) async fn connect_to_peers(
        &mut self,
        peer_addrs: &[aria2_protocol::bittorrent::peer::connection::PeerAddr],
        info_hash_raw: &[u8; 20],
        info_hash_v2: Option<[u8; 32]>,
        num_pieces: u32,
        piece_length: u32,
        total_size: u64,
    ) -> Result<Vec<BtPeerConn>> {
        let connection_options = {
            let group = self.group.recover();
            let mut options =
                BtPeerConnectionOptions::from_download_options(group.options(), self.local_peer_id);
            options.hybrid_info_hash_v2 = info_hash_v2;
            options
        };

        // Generate our local peer ID for this session. This is used for
        // self-connection detection (C++ bittorrent::getStaticPeerId()).
        // Note: C++ generates the static peer ID once per session; here we
        // generate it at connection time. For future sessions, this should
        // be a per-session singleton.
        let local_peer_id = self.local_peer_id;

        let max_peers = self.group.recover().options().bt_max_peers;
        self.peer_coordinator.set_max_peers(max_peers);
        self.bt_runtime.set_max_peers(max_peers);
        self.bt_runtime.set_connections(0);
        let remaining_slots = if self.bt_runtime.less_than_max_peers() {
            max_peers.saturating_sub(self.bt_runtime.connections())
        } else {
            0
        };
        let peer_limit = if max_peers == 0 {
            peer_addrs.len()
        } else {
            remaining_slots.min(peer_addrs.len())
        };
        let mut eligible_peers = Vec::with_capacity(peer_limit);
        for peer in peer_addrs.iter().take(peer_limit) {
            if let Ok(ip) = peer.ip.parse::<std::net::IpAddr>()
                && self.is_peer_temporarily_rejected(&ip.to_string())
            {
                tracing::debug!(peer = %ip, port = peer.port, "Skipping temporarily rejected peer");
                continue;
            }
            eligible_peers.push(peer.clone());
        }

        let mut seen = HashSet::with_capacity(eligible_peers.len());
        eligible_peers.retain(|peer| seen.insert((peer.ip.clone(), peer.port)));
        let caretaker_id = self.group.recover().gid().value();
        let mut checked_out = Vec::with_capacity(eligible_peers.len());
        {
            let mut storage = self
                .peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for peer in eligible_peers {
                let entry =
                    crate::engine::bt_peer_storage::PeerEntry::new(peer.ip.clone(), peer.port);
                if let Some(checked_peer) = storage.add_and_checkout_peer(entry, caretaker_id) {
                    checked_out.push((peer, checked_peer));
                }
            }
        }
        let eligible_peers: Vec<_> = checked_out.iter().map(|(peer, _)| peer.clone()).collect();
        if eligible_peers.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "No available peers after PeerStorage checkout".into(),
                },
            ));
        }

        if self.group.recover().is_halt_requested() {
            self.return_checked_out_peers(&checked_out);
            self.persist_checkpoint_for_halt().await?;
            self.announce_stopped_for_halt(info_hash_raw, total_size)
                .await;
            return Err(Aria2Error::DownloadFailed(
                "BitTorrent download halted".into(),
            ));
        }

        // Keep the initial connection batch cancellable. A tracker can return
        // many slow peers, and waiting for every handshake would otherwise
        // delay graceful shutdown while new sockets continue to open.
        let lifecycle_notify = self.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        tokio::pin!(lifecycle_wait);
        let mut connect_future = Box::pin(BtPeerInteraction::connect_to_peers(
            &eligible_peers,
            info_hash_raw,
            num_pieces,
            piece_length,
            total_size,
            &connection_options,
            self.utp_socket.clone(),
        ));
        let conn_result = loop {
            tokio::select! {
                result = &mut connect_future => break result,
                _ = &mut lifecycle_wait => {
                    if self.group.recover().is_halt_requested() {
                        self.return_checked_out_peers(&checked_out);
                        self.persist_checkpoint_for_halt().await?;
                        self.announce_stopped_for_halt(info_hash_raw, total_size)
                            .await;
                        return Err(Aria2Error::DownloadFailed(
                            "BitTorrent download halted".into(),
                        ));
                    }
                    lifecycle_wait.set(lifecycle_notify.notified());
                }
            }
        };
        let conn_result = match conn_result {
            Ok(result) => result,
            Err(error) => {
                self.return_checked_out_peers(&checked_out);
                return Err(error);
            }
        };

        let mut active_connections = conn_result.connections;
        for conn in &mut active_connections {
            self.apply_peer_exchange_policy(conn);
        }

        tracing::info!("[BT] Active connections: {}", active_connections.len());

        // Filter out self-connections and duplicate peer IDs.
        // Mirrors C++ DefaultBtInteractive::receiveHandshake() checks.
        let removed = filter_duplicate_peer_connections(&mut active_connections, &local_peer_id);
        if removed > 0 {
            tracing::info!(
                "[BT] Filtered {} invalid connections (self/duplicate), {} remaining",
                removed,
                active_connections.len()
            );
        }
        self.reconcile_checked_out_peers(&checked_out, &active_connections);
        self.bt_runtime.set_connections(active_connections.len());
        self.group
            .recover()
            .set_bt_connection_count(active_connections.len());

        if active_connections.is_empty() {
            return Err(Aria2Error::Recoverable(
                RecoverableError::TemporaryNetworkFailure {
                    message: "All peer connections failed or were filtered".into(),
                },
            ));
        }

        {
            let options = self.group.recover().options_arc();
            let config = ChokingConfig {
                max_upload_slots: options.bt_max_upload_slots.unwrap_or(4) as usize,
                optimistic_unchoke_interval_secs: options
                    .bt_optimistic_unchoke_interval
                    .unwrap_or(30),
                snubbed_timeout_secs: options.bt_snubbed_timeout.unwrap_or(60),
                choke_rotation_interval_secs: 10,
            };

            let mut algo = ChokingAlgorithm::new(config);

            for conn in &active_connections {
                let Ok(ip) = conn.ip_addr.parse::<std::net::IpAddr>() else {
                    warn!(peer = %conn.ip_addr, port = conn.port, "Skipping active peer with invalid IP");
                    continue;
                };
                let socket_addr = std::net::SocketAddr::new(ip, conn.port);
                let peer_stats = PeerStats::new(conn.peer_id.unwrap_or([0u8; 20]), socket_addr);
                algo.add_peer(peer_stats);
            }

            self.choking_algo = Some(algo);
            tracing::info!(
                "[BT] Choking algorithm initialized with {} peers",
                self.choking_algo.as_ref().unwrap().len()
            );
        }

        Ok(active_connections)
    }
}
