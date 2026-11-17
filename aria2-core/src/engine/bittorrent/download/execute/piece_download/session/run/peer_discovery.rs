use tracing::info;

use crate::request::request_group::BtPeerSource;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::PieceDownloadSession;
use super::super::peer_dials::{PeerDialConfig, PeerDialQueue};

impl PieceDownloadSession<'_> {
    pub(super) fn admit_pending_tracker_peers(
        &mut self,
        peer_dials: &mut PeerDialQueue,
        peer_dial_config: &PeerDialConfig,
    ) {
        let tracker_peers = std::mem::take(&mut self.pending_tracker_peers);
        if !tracker_peers.is_empty() {
            self.queue_discovered_swarm_peers(
                &tracker_peers,
                BtPeerSource::Tracker,
                peer_dials,
                peer_dial_config,
            );
        }
    }

    pub(super) async fn update_peer_discovery(
        &mut self,
        peer_dials: &mut PeerDialQueue,
        peer_dial_config: &PeerDialConfig,
    ) {
        let endgame_candidates = self.piece_picker.endgame_candidates();
        if !endgame_candidates.is_empty() && !self.endgame_state.is_endgame_active() {
            self.endgame_state.enter_endgame();
            info!(
                "[BT] Endgame mode activated: {}/{} pieces remaining",
                endgame_candidates.len(),
                self.num_pieces
            );
        } else if endgame_candidates.is_empty() && self.endgame_state.is_endgame_active() {
            self.endgame_state.exit_endgame();
        }

        self.check_and_mark_swarm_peers_snubbed();
        {
            let group = self.command.group.recover();
            super::super::super::sync_peer_snapshots_with_swarm(&group, self.swarm);
        }

        super::super::super::super::pex::send_periodic_pex_to_swarm(
            self.swarm,
            self.last_pex_send,
            self.command.peer_exchange_enabled(),
        )
        .await;

        let pex_peers = std::mem::take(&mut self.pending_pex_peers);
        if self.command.peer_exchange_enabled() && !pex_peers.is_empty() {
            info!(
                "[PEX] Drained {} inbound peers from connections, attempting to connect",
                pex_peers.len()
            );
            self.queue_discovered_swarm_peers(
                &pex_peers,
                BtPeerSource::Pex,
                peer_dials,
                peer_dial_config,
            );
        }

        self.command.update_tracker_peer_state(self.swarm.len());
        self.command.dht_periodic_lookup.set_peer_limits(
            self.command.bt_runtime.min_peers(),
            self.command.bt_runtime.max_peers(),
        );
        let mut dht_peers = Vec::new();
        super::super::super::super::check_periodic_dht_lookup(
            &mut self.command.dht_periodic_lookup,
            &self.command.dht_engines,
            &self.meta.network_info_hash(),
            self.command.listen_port,
            self.swarm.len(),
            &mut dht_peers,
        )
        .await;
        dht_peers.retain(|peer| !self.command.is_peer_temporarily_rejected(&peer.ip));
        if !dht_peers.is_empty() {
            info!(
                discovered = dht_peers.len(),
                "[BT] Periodic DHT lookup found new peers"
            );
            self.queue_discovered_swarm_peers(
                &dht_peers,
                BtPeerSource::Dht,
                peer_dials,
                peer_dial_config,
            );
        }
        if self
            .command
            .dht_periodic_lookup
            .is_lookup_completion_pending()
        {
            self.command
                .dht_periodic_lookup
                .on_lookup_completed(self.command.tracked_peer_count());
        }
    }
}
