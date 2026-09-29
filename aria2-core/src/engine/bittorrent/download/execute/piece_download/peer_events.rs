use std::time::{Duration, Instant};

use crate::engine::bittorrent::download::command::BtDownloadCommand;
use crate::engine::bittorrent::peer::message_handler::{PeerEvent, PeerSwarm};
use crate::util::rwlock_ext::RwLockRecover;

pub(super) enum PeerWaitEvent {
    Incoming(crate::engine::bittorrent::peer::listener::IncomingPeer),
    Actor(PeerEvent),
    Wake,
}

impl BtDownloadCommand {
    /// Wait for torrent-scoped actor, listener, lifecycle, or protocol-deadline
    /// events. The event lease restores the receiver if this future is cancelled.
    pub(super) async fn wait_for_swarm_peer_event(
        &mut self,
        swarm: &mut PeerSwarm,
        deadline: Instant,
    ) -> PeerWaitEvent {
        let completion_notify = self.dht_periodic_lookup.completion_notifier();
        let completion_wait = completion_notify.notified();
        let lifecycle_notify = self.group.recover().lifecycle_notifier();
        let lifecycle_wait = lifecycle_notify.notified();
        let incoming_receiver = self.incoming_peers.as_ref().cloned();
        let mut incoming_closed = false;
        let mut event_lease = swarm.lease_event_receiver();
        let deadline_wait = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline));
        tokio::pin!(deadline_wait);

        let event = tokio::select! {
            incoming = async {
                match incoming_receiver.as_ref() {
                    Some(receiver) => receiver.lock().await.recv().await,
                    None => std::future::pending::<Option<crate::engine::bittorrent::peer::listener::IncomingPeer>>().await,
                }
            } => match incoming {
                Some(incoming) => PeerWaitEvent::Incoming(incoming),
                None => {
                    incoming_closed = true;
                    PeerWaitEvent::Wake
                }
            },
            peer_event = async {
                match event_lease.as_mut() {
                    Some(lease) => lease.recv().await,
                    None => std::future::pending::<Option<PeerEvent>>().await,
                }
            } => peer_event.map_or(PeerWaitEvent::Wake, PeerWaitEvent::Actor),
            _ = completion_wait => PeerWaitEvent::Wake,
            _ = lifecycle_wait => PeerWaitEvent::Wake,
            _ = &mut deadline_wait => PeerWaitEvent::Wake,
        };

        if incoming_closed {
            self.incoming_peers = None;
        }
        event
    }

    pub(super) fn next_peer_event_deadline(
        &self,
        connected_peer_count: usize,
        stop_timeout_deadline: Option<Instant>,
    ) -> Instant {
        let now = Instant::now();
        let mut deadline = now + Duration::from_secs(24 * 60 * 60);

        if !self.dht_engines.is_empty()
            && let Some(delay) = self
                .dht_periodic_lookup
                .next_lookup_delay(connected_peer_count)
        {
            deadline = deadline.min(now + delay);
        }
        if let Some(stop_timeout_deadline) = stop_timeout_deadline {
            deadline = deadline.min(stop_timeout_deadline);
        }
        deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bittorrent::download::command_tests::build_test_torrent;
    use crate::engine::bittorrent::peer::connection::BtPeerConn;
    use crate::engine::bittorrent::peer::upload_session::{
        BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
    };
    use crate::request::request_group::{DownloadOptions, GroupId};
    use aria2_protocol::bittorrent::message::{handshake::Handshake, types::BtMessage};
    use aria2_protocol::bittorrent::peer::connection::{PeerAddr, PeerConnection};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn cancelled_swarm_wait_preserves_incoming_receiver_and_event_lease() {
        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut command = BtDownloadCommand::new(GroupId::new(7101), &torrent, &options, None)
            .expect("test torrent should construct");
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        command.incoming_peers = Some(std::sync::Arc::new(tokio::sync::Mutex::new(receiver)));
        let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(8);
        let event_sender = swarm.event_sender().unwrap();

        let result = tokio::time::timeout(
            Duration::from_millis(10),
            command.wait_for_swarm_peer_event(&mut swarm, Instant::now() + Duration::from_secs(60)),
        )
        .await;
        assert!(result.is_err(), "the idle wait should have been cancelled");
        assert!(command.incoming_peers.is_some());
        assert!(!sender.is_closed());
        assert!(swarm.lease_event_receiver().is_some());
        assert!(!event_sender.is_closed());
    }

    #[tokio::test]
    async fn swarm_wait_receives_peer_actor_events_and_updates_registry() {
        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut command = BtDownloadCommand::new(GroupId::new(7103), &torrent, &options, None)
            .expect("test torrent should construct");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let remote_task = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0x61; 20], false, false),
            endpoint,
        );
        connection.configure_upload_with_auto_unchoke(
            &BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            1,
            16,
            false,
        );
        let actor_id = connection.actor_id;
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(8);
        assert!(
            swarm
                .spawn_peer(connection, None, Arc::clone(&provider))
                .is_ok()
        );

        let mut remote = PeerConnection::from_stream_with_peer(
            remote_task.await.unwrap(),
            [0x62; 20],
            false,
            false,
        );
        remote.send_message(&BtMessage::Interested).await.unwrap();

        let event = tokio::time::timeout(
            Duration::from_secs(1),
            command.wait_for_swarm_peer_event(&mut swarm, Instant::now() + Duration::from_secs(1)),
        )
        .await
        .expect("peer actor event timed out");
        assert!(matches!(
            event,
            PeerWaitEvent::Actor(crate::engine::bittorrent::peer::message_handler::PeerEvent::InterestChanged {
                actor_id: event_actor_id,
                snapshot,
            }) if event_actor_id == actor_id && snapshot.peer_interested
        ));
        assert!(swarm.actor(actor_id).unwrap().stats.peer_interested);

        swarm.shutdown_all().await;
    }

    #[tokio::test]
    async fn actor_interest_changes_do_not_disconnect_peer() {
        let torrent = build_test_torrent();
        let options = DownloadOptions::default();
        let mut command = BtDownloadCommand::new(GroupId::new(7102), &torrent, &options, None)
            .expect("test torrent should construct");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let info_hash = [0x51; 20];
        let remote = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut handshake = [0; 68];
            stream.read_exact(&mut handshake).await.unwrap();
            stream
                .write_all(&Handshake::new(&info_hash, &[0x52; 20]).to_bytes())
                .await
                .unwrap();
            stream
        });
        let mut connection = BtPeerConn::connect_plain_with_policy(
            &PeerAddr::new("127.0.0.1", address.port()),
            &info_hash,
            None,
            &[0x53; 20],
            Duration::from_secs(5),
            false,
            &crate::network::OutboundNetworkPolicy::direct(),
        )
        .await
        .unwrap();
        connection.configure_upload_with_auto_unchoke(
            &BtSeedingConfig::default(),
            crate::rate_limiter::RateLimiter::unlimited(),
            1,
            16,
            false,
        );
        let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
        let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(8);
        let actor_id = swarm
            .spawn_peer(connection, None, provider)
            .unwrap_or_else(|_| panic!("peer actor should spawn"));
        let mut remote_connection =
            PeerConnection::from_stream_with_peer(remote.await.unwrap(), [0x54; 20], false, false);
        for (message, expected_interested) in [
            (BtMessage::Interested, true),
            (BtMessage::NotInterested, false),
        ] {
            remote_connection.send_message(&message).await.unwrap();
            let event = tokio::time::timeout(
                Duration::from_secs(5),
                command
                    .wait_for_swarm_peer_event(&mut swarm, Instant::now() + Duration::from_secs(5)),
            )
            .await
            .expect("peer actor interest event timed out");
            assert!(matches!(
                event,
                PeerWaitEvent::Actor(crate::engine::bittorrent::peer::message_handler::PeerEvent::InterestChanged {
                    actor_id: event_actor_id,
                    snapshot,
                }) if event_actor_id == actor_id && snapshot.peer_interested == expected_interested
            ));
            assert!(!swarm.actor(actor_id).unwrap().dead);
        }
        swarm.shutdown_all().await;
    }
}
