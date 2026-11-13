use std::sync::Arc;

use tracing::{debug, info};

use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::message_handler::PeerCommand;
use crate::util::rwlock_ext::RwLockRecover;

use super::super::BtSeedManager;

impl BtSeedManager {
    pub(super) fn start_seed_peer_actors(&mut self) {
        let connections = std::mem::take(&mut self.pending_connections);
        let dht_engines = self
            .peer_discovery
            .as_ref()
            .map(|discovery| discovery.dht_engines.clone());
        for mut connection in connections {
            self.prepare_actor_startup(&mut connection);
            let dht_engine = connection
                .remote_endpoint()
                .and_then(|endpoint| dht_engines.as_ref()?.for_peer(endpoint));
            if let Err(connection) =
                self.swarm
                    .spawn_peer(connection, dht_engine, Arc::clone(&self.piece_provider))
            {
                let endpoint = connection.remote_endpoint();
                drop(connection);
                if let Some(endpoint) = endpoint {
                    self.release_peer(endpoint);
                }
            }
        }
    }

    fn prepare_actor_startup(&self, connection: &mut BtPeerConn) {
        let (peer_agent, listen_port) = self
            .peer_discovery
            .as_ref()
            .map(|discovery| {
                (
                    discovery.connection_options.peer_agent.clone(),
                    discovery.connection_options.listen_port,
                )
            })
            .unwrap_or_else(|| {
                (
                    aria2_protocol::identity::DEFAULT_PEER_AGENT.to_string(),
                    None,
                )
            });
        connection.prepare_actor_startup(
            peer_agent,
            listen_port,
            &self.info_hash,
            self.piece_provider.num_pieces(),
        );
    }

    pub(in crate::engine::bittorrent::download::seed_manager) async fn drain_incoming_peers(
        &mut self,
    ) {
        loop {
            let Some(receiver) = self.incoming_peers.as_ref().cloned() else {
                return;
            };
            let incoming = receiver.lock().await.try_recv();
            match incoming {
                Ok(incoming) => self.admit_incoming_peer(incoming).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.incoming_peers = None;
                    return;
                }
            }
        }
    }

    pub(super) async fn admit_incoming_peer(
        &mut self,
        incoming: crate::engine::bittorrent::peer::listener::IncomingPeer,
    ) {
        let endpoint = incoming.endpoint;
        if let Some(discovery) = self.peer_discovery.as_ref() {
            let max_peers = discovery.group.recover().options().bt_max_peers;
            let pending = self
                .pending_peer_connection
                .as_ref()
                .map_or(0, |attempt| attempt.checked_out.len());
            if max_peers > 0
                && self.swarm.len() + self.pending_connections.len() + pending >= max_peers
            {
                debug!(%endpoint, max_peers, "Rejected incoming seeding peer at the configured peer limit");
                drop(incoming);
                self.release_peer(endpoint);
                return;
            }
        }
        let mut connection = BtPeerConn::from_incoming(incoming.connection, endpoint);
        if let Some(discovery) = self.peer_discovery.as_ref() {
            connection.set_timeouts(
                discovery.connection_options.keep_alive_interval,
                discovery.connection_options.peer_timeout,
            );
        }
        if let Some(discovery) = self.peer_discovery.as_ref() {
            connection.set_pex_enabled(discovery.peer_exchange_allowed);
            connection.set_peer_exchange_option(discovery.peer_exchange_option());
        } else {
            connection.set_pex_enabled(false);
        }
        connection.configure_upload_with_auto_unchoke(
            &self.config,
            self.torrent_upload_limiter.clone(),
            self.piece_provider.num_pieces(),
            self.piece_provider.piece_length(),
            false,
        );
        self.prepare_actor_startup(&mut connection);
        connection.set_upload_counter(std::sync::Arc::clone(&self.upload_counter));
        connection.stats.am_choking = true;
        let remote_peer_id = connection.remote_peer_id();
        let duplicate = remote_peer_id == Some(self.peer_id)
            || remote_peer_id.is_some_and(|peer_id| self.swarm.has_peer_id(peer_id))
            || self.swarm.has_endpoint(endpoint);

        if duplicate {
            debug!(%endpoint, remote_peer_id = ?remote_peer_id, "Rejected duplicate or self BitTorrent seed peer");
            self.release_peer(endpoint);
            return;
        }

        let dht_engines = self
            .peer_discovery
            .as_ref()
            .map(|discovery| discovery.dht_engines.clone());

        let Some(mut coordinator) = self.swarm.lease_event_receiver() else {
            self.release_peer(endpoint);
            self.publish_connection_state();
            return;
        };
        let actor_id = match coordinator.spawn_peer(
            connection,
            dht_engines
                .as_ref()
                .and_then(|engines| engines.for_peer(endpoint)),
            Arc::clone(&self.piece_provider),
        ) {
            Ok(actor_id) => actor_id,
            Err(connection) => {
                drop(coordinator);
                drop(connection);
                self.release_peer(endpoint);
                self.publish_connection_state();
                return;
            }
        };
        if let Some(actor) = coordinator.actor_mut(actor_id)
            && let Err(error) = actor.try_send(PeerCommand::AnnounceAvailability)
        {
            actor.dead = true;
            debug!(%endpoint, %error, "Failed to queue seed peer availability announcement");
        }
        drop(coordinator);
        self.publish_connection_state();
        info!(%endpoint, "Admitted incoming BitTorrent seed peer");
    }

    pub(super) fn release_peer(&mut self, endpoint: std::net::SocketAddr) {
        self.peer_sources.remove(&endpoint);
        if let Some(peer_storage) = &self.peer_storage {
            peer_storage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .return_peer_by_endpoint(&endpoint.ip().to_string(), endpoint.port());
        }
    }
}
