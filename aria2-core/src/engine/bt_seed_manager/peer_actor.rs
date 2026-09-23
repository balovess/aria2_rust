//! Seeding ownership wrapper around the download path's shared peer worker.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;

pub(super) use crate::engine::bt_message_handler::{PeerActorTask, PeerCommand, PeerEvent};
use crate::engine::bt_peer_connection::BtPeerConn;
pub(super) use crate::engine::bt_peer_connection::PeerActorId;
use crate::engine::bt_upload_session::PieceDataProvider;

pub(super) struct SeedPeerActor {
    pub(super) actor_id: PeerActorId,
    pub(super) endpoint: SocketAddr,
    pub(super) dead: bool,
    actor: PeerActorTask,
}

impl SeedPeerActor {
    pub(super) fn spawn(
        actor_id: PeerActorId,
        connection: BtPeerConn,
        provider: Arc<dyn PieceDataProvider>,
        event_tx: mpsc::Sender<PeerEvent>,
    ) -> Self {
        let endpoint = format!("{}:{}", connection.remote_ip(), connection.remote_port())
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        let actor =
            PeerActorTask::spawn_owned(actor_id, connection, event_tx, None, Some(provider), 16);

        Self {
            actor_id,
            endpoint,
            dead: false,
            actor,
        }
    }

    pub(super) async fn send(
        &self,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::SendError<PeerCommand>> {
        self.actor.control.send(command).await
    }

    pub(super) async fn shutdown(&mut self) {
        let _ = self.actor.shutdown().await;
    }
}
