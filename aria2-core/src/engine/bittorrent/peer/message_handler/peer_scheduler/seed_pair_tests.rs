use super::{PeerEvent, PeerSwarm};
use crate::engine::bittorrent::peer::upload_session::{InMemoryPieceProvider, PieceDataProvider};
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn two_seeders_disconnect_without_creating_a_pex_drop() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_socket = tokio::net::TcpSocket::new_v4().unwrap();
    remote_socket
        .bind(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 0))
        .unwrap();
    let remote_stream = remote_socket
        .connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let connection = PeerConnection::from_stream_with_peer_capabilities(
        local_stream,
        [1; 20],
        false,
        false,
        true,
    );
    let mut connection = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        connection, endpoint,
    );
    connection.incoming = false;
    connection.allocate_session_resource(16, 1, 16);
    let actor_id = connection.actor_id;
    let advertised_endpoint = SocketAddr::new(endpoint.ip(), 6881);
    connection.remote_listen_port = Some(6881);

    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(4);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    let mut remote = PeerConnection::from_stream_with_peer_capabilities(
        remote_stream,
        [2; 20],
        false,
        false,
        true,
    );
    remote
        .send_message(&BtMessage::Bitfield { data: vec![0x80] })
        .await
        .unwrap();

    {
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await,
                    Some(PeerEvent::PeerAvailabilitySnapshot {
                        actor_id: event_actor,
                        seeder: true,
                        ..
                    }) if event_actor == actor_id
                ) {
                    break;
                }
            }
        })
        .await
        .expect("the remote peer's full bitfield should mark it as a seeder");
    }

    swarm.set_local_seeder(true);
    {
        let mut events = swarm.lease_event_receiver().unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), events.recv())
                .await
                .expect("the local seeder state should close a seed-to-seed connection"),
            Some(PeerEvent::Disconnected { actor_id: event_actor }) if event_actor == actor_id
        ));
    }

    let removed = swarm.remove_dead().await;
    assert!(removed.contains(&(actor_id, endpoint)));
    assert!(
        !swarm
            .recently_dropped_endpoints()
            .any(|(dropped, _)| dropped == advertised_endpoint),
        "the original seed-to-seed close is not a graceful PEX dropped event"
    );
    swarm.shutdown_all().await;
    drop(remote);
}
