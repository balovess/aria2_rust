use tracing::info;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::engine::bt_message_handler::{PeerCommand, PeerSwarm};
use crate::engine::bt_upload_session::PieceDataProvider;
use crate::util::rwlock_ext::RwLockRecover;

pub(super) struct PeerActorUploadContext {
    pub(super) provider: std::sync::Arc<dyn PieceDataProvider>,
    pub(super) upload_counter: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl BtDownloadCommand {
    pub(super) fn admit_incoming_peer_to_swarm(
        &mut self,
        swarm: &mut PeerSwarm,
        incoming: crate::engine::bt_peer_listener::IncomingPeer,
        piece_length: u32,
        num_pieces: u32,
        total_size: u64,
        upload: &PeerActorUploadContext,
    ) -> bool {
        let endpoint = incoming.endpoint;
        let mut connection = match incoming.connection {
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(connection) => {
                crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
                    *connection,
                    endpoint,
                )
            }
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                connection,
            ) => crate::engine::bt_peer_connection::BtPeerConn::from_incoming_encrypted(
                *connection,
                endpoint,
            ),
        };
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

        connection.allocate_session_resource(piece_length, num_pieces, total_size);
        self.configure_upload_connection(&mut connection, piece_length, num_pieces);
        connection.set_upload_counter(std::sync::Arc::clone(&upload.upload_counter));
        connection.set_upload_progress(std::sync::Arc::clone(&self.progress));
        self.track_peer_for_upload_choking(&connection.stats);
        let actor_id = match swarm.spawn_peer(
            connection,
            self.dht_engine.clone(),
            std::sync::Arc::clone(&upload.provider),
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

    pub(super) fn drain_incoming_peers_to_swarm(
        &mut self,
        swarm: &mut PeerSwarm,
        piece_length: u32,
        num_pieces: u32,
        total_size: u64,
        upload: PeerActorUploadContext,
    ) -> usize {
        let Some(mut receiver) = self.incoming_peers.take() else {
            return 0;
        };
        let mut admitted = 0;
        while let Ok(incoming) = receiver.try_recv() {
            admitted += usize::from(self.admit_incoming_peer_to_swarm(
                swarm,
                incoming,
                piece_length,
                num_pieces,
                total_size,
                &upload,
            ));
        }
        self.incoming_peers = Some(receiver);
        admitted
    }

    /// Stage handshaken peers until the torrent's upload provider and swarm are ready.
    pub(super) fn stage_incoming_peers(
        &mut self,
        active_connections: &mut Vec<crate::engine::bt_peer_connection::BtPeerConn>,
        piece_length: u32,
        num_pieces: u32,
        total_size: u64,
    ) {
        let Some(mut receiver) = self.incoming_peers.take() else {
            return;
        };
        while let Ok(incoming) = receiver.try_recv() {
            self.stage_incoming_peer(
                active_connections,
                incoming,
                piece_length,
                num_pieces,
                total_size,
            );
        }
        self.incoming_peers = Some(receiver);
    }

    fn stage_incoming_peer(
        &mut self,
        active_connections: &mut Vec<crate::engine::bt_peer_connection::BtPeerConn>,
        incoming: crate::engine::bt_peer_listener::IncomingPeer,
        piece_length: u32,
        num_pieces: u32,
        total_size: u64,
    ) {
        let endpoint = incoming.endpoint;
        let mut conn = match incoming.connection {
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Plain(connection) => {
                crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
                    *connection,
                    endpoint,
                )
            }
            aria2_protocol::bittorrent::peer::incoming::IncomingConnection::Encrypted(
                connection,
            ) => crate::engine::bt_peer_connection::BtPeerConn::from_incoming_encrypted(
                *connection,
                endpoint,
            ),
        };
        self.apply_peer_exchange_policy(&mut conn);
        let remote_peer_id = conn.remote_peer_id();
        if remote_peer_id == Some(self.local_peer_id)
            || remote_peer_id.is_some_and(|peer_id| {
                active_connections
                    .iter()
                    .any(|active| active.peer_id == Some(peer_id))
            })
        {
            self.peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
            info!(%endpoint, "Rejected incoming self or duplicate BitTorrent peer");
            return;
        }
        if !self.should_admit_incoming_peer(active_connections.len()) {
            self.peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
            info!(
                %endpoint,
                "Rejected incoming BitTorrent peer because peer speed is above the request threshold"
            );
            return;
        }
        conn.allocate_session_resource(piece_length, num_pieces, total_size);
        self.configure_upload_connection(&mut conn, piece_length, num_pieces);
        self.track_peer_for_upload_choking(&conn.stats);
        active_connections.push(conn);
        self.bt_runtime.set_connections(active_connections.len());
        self.group
            .recover()
            .set_bt_connection_count(active_connections.len());
        info!("[BT] Admitted incoming peer {}", endpoint);
    }

    pub(in crate::engine::bt_download_execute::execute) fn configure_upload_connection(
        &self,
        connection: &mut crate::engine::bt_peer_connection::BtPeerConn,
        piece_length: u32,
        num_pieces: u32,
    ) {
        let max_upload_bytes_per_sec = self.group.recover().options().max_upload_limit;
        let config = crate::engine::bt_upload_session::BtSeedingConfig {
            max_upload_bytes_per_sec,
            global_limiter: self.global_limiter.clone(),
            max_peers_to_unchoke: 4,
            optimistic_unchoke_interval_secs: 30,
        };
        connection.configure_upload_with_auto_unchoke(
            &config,
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
