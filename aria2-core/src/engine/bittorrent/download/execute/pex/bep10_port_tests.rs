use super::{PEX_SEND_INTERVAL, send_periodic_pex_to_swarm};
use crate::engine::bittorrent::peer::message_handler::{PeerEvent, PeerSwarm};
use crate::engine::bittorrent::peer::upload_session::{InMemoryPieceProvider, PieceDataProvider};
use aria2_protocol::bittorrent::extension::pex::{PexHandler, PexMessage};
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test]
async fn bep10_listen_port_is_used_for_pex_but_transport_endpoint_for_cleanup() {
    async fn incoming_peer(
        peer_id: [u8; 20],
    ) -> (
        crate::engine::bittorrent::peer::connection::BtPeerConn,
        tokio::net::TcpStream,
        std::net::SocketAddr,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (local, transport_endpoint) = listener.accept().await.unwrap();
        let connection =
            PeerConnection::from_stream_with_peer_capabilities(local, peer_id, false, false, true);
        let mut connection =
            crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
                connection,
                transport_endpoint,
            );
        connection.allocate_session_resource(16, 1, 16);
        (connection, remote, transport_endpoint)
    }

    async fn read_pex(remote: &mut PeerConnection) -> PexMessage {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let message = remote
                    .read_message()
                    .await
                    .expect("PEX actor stream should remain readable")
                    .expect("PEX actor should not close the connection");
                if let BtMessage::Extended {
                    ext_id: 19,
                    payload,
                } = message
                {
                    return PexHandler::parse_pex_data(&payload)
                        .expect("actor should emit a valid PEX payload");
                }
            }
        })
        .await
        .expect("actor should send the PEX message before the deadline")
    }

    let (first_connection, first_stream, first_transport) = incoming_peer([1; 20]).await;
    let (second_connection, second_stream, second_transport) = incoming_peer([2; 20]).await;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    let first_actor = first_connection.actor_id;
    let second_actor = second_connection.actor_id;
    assert!(
        swarm
            .spawn_peer(first_connection, None, Arc::clone(&provider))
            .is_ok()
    );
    assert!(swarm.spawn_peer(second_connection, None, provider).is_ok());

    let mut first_remote = PeerConnection::from_stream_with_peer_capabilities(
        first_stream,
        [11; 20],
        false,
        false,
        true,
    );
    let mut second_remote = PeerConnection::from_stream_with_peer_capabilities(
        second_stream,
        [12; 20],
        false,
        false,
        true,
    );
    for (remote, port) in [(&mut first_remote, 6881), (&mut second_remote, 6882)] {
        let mut handshake =
            aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new();
        handshake.with_ut_pex(19).with_port(port);
        remote
            .send_message(&BtMessage::Extended {
                ext_id: 0,
                payload: handshake.to_bytes(),
            })
            .await
            .unwrap();
    }

    {
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut negotiated = HashSet::new();
            while negotiated.len() < 2 {
                let event = events
                    .recv()
                    .await
                    .expect("swarm event channel should stay open");
                if let PeerEvent::ExtensionHandshakeReceived {
                    actor_id,
                    ut_pex_id,
                    remote_listen_port,
                } = event
                {
                    assert_eq!(ut_pex_id, Some(19));
                    assert!(
                        matches!(
                            (actor_id, remote_listen_port),
                            (id, Some(6881)) if id == first_actor
                        ) || matches!(
                            (actor_id, remote_listen_port),
                            (id, Some(6882)) if id == second_actor
                        )
                    );
                    negotiated.insert(actor_id);
                }
            }
        })
        .await
        .expect("both BEP 10 handshakes should reach their peer actors");
    }

    let first_advertised = std::net::SocketAddr::new(first_transport.ip(), 6881);
    let second_advertised = std::net::SocketAddr::new(second_transport.ip(), 6882);
    assert_eq!(
        swarm.actor(first_actor).unwrap().advertised_endpoint,
        Some(first_advertised)
    );
    assert_eq!(
        swarm.actor(second_actor).unwrap().advertised_endpoint,
        Some(second_advertised)
    );
    assert!(!swarm.actor(first_actor).unwrap().incoming);
    assert!(!swarm.actor(second_actor).unwrap().incoming);
    assert!(swarm.has_endpoint(first_transport));
    assert!(swarm.has_endpoint(second_transport));
    assert!(!swarm.has_endpoint(first_advertised));
    assert!(!swarm.has_endpoint(second_advertised));

    let mut last_pex_send = Instant::now() - PEX_SEND_INTERVAL;
    send_periodic_pex_to_swarm(&mut swarm, &mut last_pex_send, true).await;
    let PexMessage::Added {
        peers: first_peers, ..
    } = read_pex(&mut first_remote).await
    else {
        panic!("expected standard BEP 11 message for first peer");
    };
    let PexMessage::Added {
        peers: second_peers,
        ..
    } = read_pex(&mut second_remote).await
    else {
        panic!("expected standard BEP 11 message for second peer");
    };
    assert!(first_peers.iter().any(|peer| peer.addr.port == 6882));
    assert!(
        !first_peers
            .iter()
            .any(|peer| peer.addr.port == second_transport.port())
    );
    assert!(second_peers.iter().any(|peer| peer.addr.port == 6881));
    assert!(
        !second_peers
            .iter()
            .any(|peer| peer.addr.port == first_transport.port())
    );

    drop(first_remote);
    {
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await,
                    Some(PeerEvent::Disconnected { actor_id }) if actor_id == first_actor
                ) {
                    break;
                }
            }
        })
        .await
        .expect("peer actor should report the closed transport");
    }
    let removed = swarm.remove_dead().await;
    assert!(removed.contains(&(first_actor, first_transport)));
    assert!(
        swarm
            .recently_dropped_endpoints()
            .any(|endpoint| endpoint == first_advertised)
    );
    assert!(
        !swarm
            .recently_dropped_endpoints()
            .any(|endpoint| endpoint == first_transport)
    );

    last_pex_send = Instant::now() - PEX_SEND_INTERVAL;
    send_periodic_pex_to_swarm(&mut swarm, &mut last_pex_send, true).await;
    let PexMessage::Added { dropped, .. } = read_pex(&mut second_remote).await else {
        panic!("expected standard BEP 11 message for second peer");
    };
    assert!(dropped.iter().any(|peer| peer.port == 6881));
    assert!(
        !dropped
            .iter()
            .any(|peer| peer.port == first_transport.port())
    );

    swarm.shutdown_all().await;
}
