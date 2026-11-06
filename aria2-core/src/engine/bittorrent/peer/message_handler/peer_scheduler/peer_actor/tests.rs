use super::*;

use super::super::pipelined::BlockRequest;
use crate::engine::bittorrent::peer::upload_session::{
    BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
};
use aria2_protocol::bittorrent::message::types::{BtMessage, PieceBlockRequest};
use aria2_protocol::bittorrent::peer::connection::PeerConnection;

async fn read_message_while_actor_runs(
    remote: &mut PeerConnection,
) -> aria2_protocol::bittorrent::message::types::BtMessage {
    use tokio::time::{Duration, timeout};

    timeout(Duration::from_secs(1), remote.read_message())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

async fn receive_event_while_actor_runs(event_rx: &mut PeerSwarmEventLease<'_>) -> PeerEvent {
    use tokio::time::{Duration, timeout};

    loop {
        let event = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .expect("peer event channel closed");
        if !matches!(event, PeerEvent::OutstandingDownloadRequests { .. }) {
            return event;
        }
    }
}

#[path = "tests/control.rs"]
mod control;
#[path = "tests/lifecycle.rs"]
mod lifecycle;
#[path = "tests/startup.rs"]
mod startup;
#[path = "tests/transport.rs"]
mod transport;
#[path = "tests/upload.rs"]
mod upload;
