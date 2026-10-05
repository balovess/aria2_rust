use tokio::sync::mpsc;

use super::super::super::types::MAX_OUTSTANDING_REQUEST;
use super::super::{PeerCommand, PeerEvent};
use super::{PeerActorEntry, PeerSwarm};

/// Temporarily lends the swarm event receiver to one coordinator without
/// losing it when that coordinator's future is cancelled.
pub(crate) struct PeerSwarmEventLease<'a> {
    pub(super) swarm: &'a mut PeerSwarm,
    pub(super) receiver: Option<mpsc::Receiver<PeerEvent>>,
}

impl PeerSwarmEventLease<'_> {
    pub(crate) async fn recv(&mut self) -> Option<PeerEvent> {
        let event = self.recv_unapplied().await?;
        self.swarm.apply_event(&event);
        Some(event)
    }

    /// Receive an event without mutating the registry. Coordinators that
    /// apply additional event-side effects can use this to apply registry
    /// state exactly once in their common event handler.
    pub(crate) async fn recv_unapplied(&mut self) -> Option<PeerEvent> {
        loop {
            let receiver = self.receiver.as_mut()?;
            let event = if let Some(deadline) = self.swarm.pending_stats_snapshot_deadline() {
                tokio::select! {
                    event = receiver.recv() => event,
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                        self.swarm.publish_pending_stats_snapshot();
                        continue;
                    }
                }
            } else {
                receiver.recv().await
            };
            return event;
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<PeerEvent, mpsc::error::TryRecvError> {
        let event = self.try_recv_unapplied()?;
        self.swarm.apply_event(&event);
        Ok(event)
    }

    pub(crate) fn try_recv_unapplied(&mut self) -> Result<PeerEvent, mpsc::error::TryRecvError> {
        self.receiver
            .as_mut()
            .ok_or(mpsc::error::TryRecvError::Disconnected)?
            .try_recv()
    }

    pub(crate) async fn send_to(
        &self,
        actor_id: crate::engine::bittorrent::peer::connection::PeerActorId,
        command: PeerCommand,
    ) -> Result<(), mpsc::error::SendError<PeerCommand>> {
        let Some(actor) = self.swarm.actor(actor_id) else {
            return Err(mpsc::error::SendError(command));
        };
        actor.handle().send(command).await
    }

    /// Admit a handshaken connection through the same coordinator lease that
    /// owns event consumption, so dynamic joins cannot race another swarm
    /// mutator while the event loop is active.
    pub(crate) fn spawn_peer(
        &mut self,
        connection: crate::engine::bittorrent::peer::connection::BtPeerConn,
        dht_engine: Option<std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine>>,
        provider: std::sync::Arc<
            dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider,
        >,
    ) -> Result<
        crate::engine::bittorrent::peer::connection::PeerActorId,
        Box<crate::engine::bittorrent::peer::connection::BtPeerConn>,
    > {
        self.swarm.spawn_peer(connection, dht_engine, provider)
    }

    pub(crate) fn actor_mut(
        &mut self,
        actor_id: crate::engine::bittorrent::peer::connection::PeerActorId,
    ) -> Option<&mut PeerActorEntry> {
        self.swarm.actor_mut(actor_id)
    }

    pub(crate) fn increase_request_window(
        &mut self,
        actor_id: crate::engine::bittorrent::peer::connection::PeerActorId,
    ) -> Option<usize> {
        let actor = self.swarm.actor_mut(actor_id)?;
        actor.max_outstanding_requests = actor
            .max_outstanding_requests
            .saturating_mul(2)
            .min(MAX_OUTSTANDING_REQUEST);
        Some(actor.max_outstanding_requests)
    }
}

impl Drop for PeerSwarmEventLease<'_> {
    fn drop(&mut self) {
        if self.swarm.event_rx.is_none() {
            self.swarm.event_rx = self.receiver.take();
        }
    }
}
