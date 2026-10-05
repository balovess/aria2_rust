use tracing::info;

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::peer::message_handler::{PeerCommand, PeerSwarm};
use crate::engine::bittorrent::peer::upload_session::PieceDataProvider;
use crate::util::rwlock_ext::RwLockRecover;

pub(super) struct PeerActorAdmissionContext {
    pub(super) network_info_hash: [u8; 20],
    pub(super) piece_length: u32,
    pub(super) num_pieces: u32,
    pub(super) total_size: u64,
    pub(super) provider: std::sync::Arc<dyn PieceDataProvider>,
    pub(super) upload_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl BtDownloadCommand {
    pub(super) fn admit_incoming_peer_to_swarm(
        &mut self,
        swarm: &mut PeerSwarm,
        incoming: crate::engine::bittorrent::peer::listener::IncomingPeer,
        context: &PeerActorAdmissionContext,
    ) -> bool {
        let endpoint = incoming.endpoint;
        let mut connection =
            crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
                incoming.connection,
                endpoint,
            );
        let (keep_alive_interval, peer_timeout) = {
            let group = self.group.recover();
            (
                std::time::Duration::from_secs(group.options().bt_keep_alive_interval),
                std::time::Duration::from_secs(group.options().bt_timeout),
            )
        };
        connection.set_timeouts(keep_alive_interval, peer_timeout);
        self.apply_peer_exchange_policy(&mut connection);

        let remote_peer_id = connection.remote_peer_id();
        if remote_peer_id == Some(self.local_peer_id)
            || remote_peer_id.is_some_and(|peer_id| swarm.has_peer_id(peer_id))
            || swarm.has_endpoint(endpoint)
        {
            self.release_peer_endpoint(endpoint);
            info!(%endpoint, "Rejected incoming self or duplicate BitTorrent peer");
            return false;
        }
        if !self.should_admit_incoming_peer(swarm.len()) {
            self.release_peer_endpoint(endpoint);
            info!(
                %endpoint,
                "Rejected incoming BitTorrent peer because peer speed is above the request threshold"
            );
            return false;
        }

        connection.allocate_session_resource(
            context.piece_length,
            context.num_pieces,
            context.total_size,
        );
        self.configure_upload_connection(&mut connection, context.piece_length, context.num_pieces);
        let (peer_agent, listen_port) = {
            let group = self.group.recover();
            (
                group.options().peer_agent.clone(),
                (self.listen_port != 0).then_some(self.listen_port),
            )
        };
        connection.prepare_actor_startup(
            peer_agent,
            listen_port,
            &context.network_info_hash,
            context.num_pieces,
        );
        connection.set_upload_counter(std::sync::Arc::clone(&context.upload_counter));
        connection.set_upload_progress(std::sync::Arc::clone(&self.progress));
        self.track_peer_for_upload_choking(&connection.stats);
        let actor_id = match swarm.spawn_peer(
            connection,
            self.dht_engines.for_peer(endpoint),
            std::sync::Arc::clone(&context.provider),
        ) {
            Ok(actor_id) => actor_id,
            Err(connection) => {
                drop(connection);
                self.release_peer_endpoint(endpoint);
                return false;
            }
        };
        if swarm
            .try_send_to(actor_id, PeerCommand::AnnounceAvailability)
            .is_err()
        {
            swarm.mark_dead(actor_id);
        }
        let count = swarm.len();
        self.bt_runtime.set_connections(count);
        self.group.recover().set_bt_connection_count(count);
        info!(%endpoint, actor_id = actor_id.0, "Admitted incoming peer into torrent Swarm");
        true
    }

    pub(super) async fn drain_incoming_peers_to_swarm(
        &mut self,
        swarm: &mut PeerSwarm,
        context: PeerActorAdmissionContext,
    ) -> usize {
        let mut admitted = 0;
        loop {
            let Some(receiver) = self.incoming_peers.as_ref().cloned() else {
                break;
            };
            let incoming = receiver.lock().await.try_recv();
            match incoming {
                Ok(incoming) => {
                    admitted +=
                        usize::from(self.admit_incoming_peer_to_swarm(swarm, incoming, &context));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.incoming_peers = None;
                    break;
                }
            }
        }
        admitted
    }

    pub(in crate::engine::bittorrent::download::execute) fn configure_upload_connection(
        &self,
        connection: &mut crate::engine::bittorrent::peer::connection::BtPeerConn,
        piece_length: u32,
        num_pieces: u32,
    ) {
        let options = self.group.recover().options_arc();
        let config = crate::engine::bittorrent::peer::upload_session::BtSeedingConfig {
            max_upload_bytes_per_sec: options.max_upload_limit,
            global_limiter: self.global_limiter.clone(),
            max_peers_to_unchoke: options
                .bt_max_upload_slots
                .unwrap_or(crate::constants::BT_DEFAULT_MAX_UPLOAD_SLOTS as u32)
                as usize,
            optimistic_unchoke_interval_secs: options
                .bt_optimistic_unchoke_interval
                .unwrap_or(crate::constants::BT_OPTIMISTIC_UNCHOKE_INTERVAL_SECS),
        };
        connection.configure_upload_with_auto_unchoke(
            &config,
            self.torrent_upload_limiter.clone(),
            num_pieces,
            piece_length,
            self.choking_algo.is_none(),
        );
    }

    pub(super) fn release_peer_endpoint(&self, endpoint: std::net::SocketAddr) {
        self.peer_storage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
    }
}
