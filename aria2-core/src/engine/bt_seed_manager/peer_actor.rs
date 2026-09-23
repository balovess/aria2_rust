//! Seeding ownership wrapper around the download path's shared peer worker.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::engine::bt_message_handler::peer_worker;
pub(super) use crate::engine::bt_message_handler::{PeerCommand, PeerEvent};
use crate::engine::bt_peer_connection::BtPeerConn;
use crate::engine::bt_upload_session::PieceDataProvider;

pub(super) struct SeedPeerActor {
    pub(super) actor_id: usize,
    pub(super) endpoint: SocketAddr,
    pub(super) dead: bool,
    command_tx: mpsc::Sender<PeerCommand>,
    task: Option<JoinHandle<usize>>,
}

impl SeedPeerActor {
    pub(super) fn spawn(
        actor_id: usize,
        mut connection: BtPeerConn,
        provider: Arc<dyn PieceDataProvider>,
        event_tx: mpsc::Sender<PeerEvent>,
    ) -> Self {
        let endpoint = format!("{}:{}", connection.remote_ip(), connection.remote_port())
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
        let (command_tx, command_rx) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            peer_worker(
                actor_id,
                &mut connection,
                command_rx,
                event_tx,
                None,
                Some(provider),
            )
            .await
        });

        Self {
            actor_id,
            endpoint,
            dead: false,
            command_tx,
            task: Some(task),
        }
    }

    pub(super) async fn send(
        &self,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::SendError<PeerCommand>> {
        self.command_tx.send(command).await
    }

    pub(super) async fn shutdown(&mut self) {
        let _ = self.command_tx.send(PeerCommand::Shutdown).await;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SeedPeerActor {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
