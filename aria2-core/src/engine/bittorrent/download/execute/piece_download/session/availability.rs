//! Apply peer actor availability events to the torrent's piece-frequency tracker.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::engine::bittorrent::download::execute::types::PeerKey;
use crate::engine::bittorrent::peer::connection::PeerActorId;
use crate::engine::bittorrent::peer::message_handler::PeerSwarm;
use crate::engine::bittorrent::piece::PeerBitfieldTracker;
use crate::engine::bittorrent::piece::PiecePicker;

pub(super) fn sync_swarm_actor_availability(
    swarm: &PeerSwarm,
    changed_actor_ids: &[PeerActorId],
    peer_tracker: &mut PeerBitfieldTracker,
    piece_picker: &mut PiecePicker,
    peer_last_data_time: &mut HashMap<PeerKey, Instant>,
) {
    let changed: HashSet<_> = changed_actor_ids.iter().copied().collect();
    let mut availability_changed = false;
    for actor in swarm.iter() {
        if actor.dead || !changed.contains(&actor.actor_id) || !actor.has_bitfield {
            continue;
        }
        let peer = actor.endpoint.to_string();
        if peer_tracker.get_peer_bitfield_raw(&peer) != Some(actor.bitfield.as_slice()) {
            peer_tracker.update_peer_bitfield(&peer, &actor.bitfield);
            peer_last_data_time.insert(PeerKey::new(actor.endpoint), Instant::now());
            availability_changed = true;
        }
    }
    if availability_changed {
        piece_picker.set_frequencies_from_peers(&peer_tracker.piece_frequencies());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::bittorrent::peer::connection::BtPeerConn;
    use crate::engine::bittorrent::peer::message_handler::PeerEvent;
    use crate::engine::bittorrent::peer::upload_session::{
        InMemoryPieceProvider, PieceDataProvider,
    };
    use aria2_protocol::bittorrent::peer::connection::PeerConnection;

    #[tokio::test]
    async fn changed_actor_updates_piece_frequency_from_swarm_state() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (local, endpoint) = listener.accept().await.unwrap();
        let mut connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local, [0; 20], false, true),
            endpoint,
        );
        connection.allocate_session_resource(16, 8, 128);
        connection.set_peer_bitfield(&[0x80]);
        let mut swarm = PeerSwarm::new(4);
        let provider: std::sync::Arc<dyn PieceDataProvider> =
            std::sync::Arc::new(InMemoryPieceProvider::new(16, 8));
        let actor_id = swarm
            .spawn_peer(connection, None, provider)
            .unwrap_or_else(|_| panic!("fresh peer must register in an empty swarm"));
        let endpoint = swarm.actor(actor_id).unwrap().endpoint;
        let tracker_key = endpoint.to_string();
        let mut tracker = PeerBitfieldTracker::new(8);
        let mut picker = PiecePicker::new(8);
        tracker.update_peer_bitfield(&tracker_key, &[0x80]);
        let mut last_data_time = HashMap::new();

        swarm
            .event_tx
            .as_ref()
            .unwrap()
            .send(PeerEvent::PeerAvailabilityChanged {
                actor_id,
                piece_index: 1,
                has_piece: true,
            })
            .await
            .unwrap();
        let mut event_lease = swarm.lease_event_receiver().unwrap();
        assert!(
            matches!(event_lease.recv().await, Some(PeerEvent::PeerAvailabilityChanged { actor_id: event_actor, .. }) if event_actor == actor_id)
        );
        drop(event_lease);
        sync_swarm_actor_availability(
            &swarm,
            &[actor_id],
            &mut tracker,
            &mut picker,
            &mut last_data_time,
        );

        assert_eq!(tracker.piece_frequencies()[..3], [1, 1, 0]);
        assert_eq!(last_data_time.len(), 1);
        swarm.shutdown_all().await;
        drop(remote);
    }
}
