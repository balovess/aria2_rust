use tracing::info;

use crate::engine::bt_download_command::BtDownloadCommand;
use crate::util::rwlock_ext::RwLockRecover;

impl BtDownloadCommand {
    pub(super) fn drain_incoming_peers(
        &mut self,
        active_connections: &mut Vec<crate::engine::bt_peer_connection::BtPeerConn>,
        piece_length: u32,
        total_size: u64,
    ) {
        let Some(mut receiver) = self.incoming_peers.take() else {
            return;
        };
        while let Ok(incoming) = receiver.try_recv() {
            self.admit_incoming_peer(active_connections, incoming, piece_length, total_size);
        }
        self.incoming_peers = Some(receiver);
    }

    pub(super) fn admit_incoming_peer(
        &mut self,
        active_connections: &mut Vec<crate::engine::bt_peer_connection::BtPeerConn>,
        incoming: crate::engine::bt_peer_listener::IncomingPeer,
        piece_length: u32,
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
        conn.allocate_session_resource(piece_length, total_size);
        active_connections.push(conn);
        self.bt_runtime.set_connections(active_connections.len());
        self.group
            .recover()
            .set_bt_connection_count(active_connections.len());
        info!("[BT] Admitted incoming peer {}", endpoint);
    }
}
