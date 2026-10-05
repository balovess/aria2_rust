use super::{PEX_SEND_INTERVAL, send_periodic_pex_to_swarm};
use crate::engine::bittorrent::peer::message_handler::{PeerEvent, PeerSwarm};
use crate::engine::bittorrent::peer::upload_session::{InMemoryPieceProvider, PieceDataProvider};
use aria2_protocol::bittorrent::extension::pex::{PexHandler, PexMessage};
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

async fn incoming_peer(
    source_ip: Ipv4Addr,
    peer_id: [u8; 20],
) -> (
    crate::engine::bittorrent::peer::connection::BtPeerConn,
    tokio::net::TcpStream,
    SocketAddr,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_socket = tokio::net::TcpSocket::new_v4().unwrap();
    remote_socket
        .bind(SocketAddr::new(source_ip.into(), 0))
        .unwrap();
    let remote = remote_socket
        .connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (local, transport_endpoint) = listener.accept().await.unwrap();
    assert_eq!(transport_endpoint.ip(), source_ip);
    let connection =
        PeerConnection::from_stream_with_peer_capabilities(local, peer_id, false, false, true);
    let mut connection = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        connection,
        transport_endpoint,
    );
    connection.allocate_session_resource(16, 1, 16);
    (connection, remote, transport_endpoint)
}

async fn send_pex_handshake(remote: &mut PeerConnection, port: Option<u16>) {
    let mut handshake = aria2_protocol::bittorrent::message::extension::ExtensionHandshake::new();
    handshake.with_ut_pex(19);
    if let Some(port) = port {
        handshake.with_port(port);
    }
    remote
        .send_message(&BtMessage::Extended {
            ext_id: 0,
            payload: handshake.to_bytes(),
        })
        .await
        .unwrap();
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

#[tokio::test]
async fn bep10_listen_port_is_used_for_pex_but_transport_endpoint_for_cleanup() {
    let (first_connection, first_stream, first_transport) =
        incoming_peer(Ipv4Addr::new(127, 0, 0, 2), [1; 20]).await;
    let (second_connection, second_stream, second_transport) =
        incoming_peer(Ipv4Addr::new(127, 0, 0, 3), [2; 20]).await;
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
    send_pex_handshake(&mut first_remote, Some(6881)).await;
    send_pex_handshake(&mut second_remote, Some(6882)).await;

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
                    ..
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

    let snapshots = swarm.peer_snapshots();
    for (peer_id, advertised_endpoint) in
        [([1; 20], first_advertised), ([2; 20], second_advertised)]
    {
        let snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.peer_id == peer_id)
            .expect("handshaken peer should be present in the RPC source snapshot");
        assert_eq!(snapshot.addr, advertised_endpoint);
        assert!(!snapshot.is_incoming);
    }

    for port in [Some(6883), None, Some(0)] {
        send_pex_handshake(&mut first_remote, port).await;
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await,
                    Some(PeerEvent::ExtensionHandshakeReceived {
                        actor_id,
                        ut_pex_id: Some(19),
                        remote_listen_port: Some(6883),
                        ..
                    }) if actor_id == first_actor
                ) {
                    break;
                }
            }
        })
        .await
        .expect("valid p update must apply, while omitted/zero p must preserve it");
    }
    let first_updated_advertised = SocketAddr::new(first_transport.ip(), 6883);
    assert_eq!(
        swarm
            .peer_snapshots()
            .into_iter()
            .find(|snapshot| snapshot.peer_id == [1; 20])
            .unwrap()
            .addr,
        first_updated_advertised
    );
    assert!(swarm.has_endpoint(first_transport));
    assert!(!swarm.has_endpoint(first_updated_advertised));

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
    assert!(
        first_peers
            .iter()
            .any(|peer| { peer.addr.ip == "127.0.0.3" && peer.addr.port == 6882 })
    );
    assert!(
        !first_peers
            .iter()
            .any(|peer| peer.addr.port == second_transport.port())
    );
    assert!(
        second_peers
            .iter()
            .any(|peer| { peer.addr.ip == "127.0.0.2" && peer.addr.port == 6883 })
    );
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
                    Some(PeerEvent::GracefulDisconnected { actor_id }) if actor_id == first_actor
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
            .peer_snapshots()
            .iter()
            .all(|snapshot| snapshot.peer_id != [1; 20])
    );
    assert!(
        swarm
            .recently_dropped_endpoints()
            .any(|(endpoint, _)| endpoint == first_updated_advertised)
    );
    assert!(
        !swarm
            .recently_dropped_endpoints()
            .any(|(endpoint, _)| endpoint == first_transport || endpoint == first_advertised)
    );

    last_pex_send = Instant::now() - PEX_SEND_INTERVAL;
    send_periodic_pex_to_swarm(&mut swarm, &mut last_pex_send, true).await;
    let PexMessage::Added { dropped, .. } = read_pex(&mut second_remote).await else {
        panic!("expected standard BEP 11 message for second peer");
    };
    assert!(
        dropped
            .iter()
            .any(|peer| peer.ip == "127.0.0.2" && peer.port == 6883)
    );
    assert!(
        !dropped
            .iter()
            .any(|peer| peer.port == first_transport.port())
    );

    swarm.shutdown_all().await;
}

#[tokio::test]
async fn pex_added_contains_only_fresh_peers_on_a_different_ip() {
    let (recipient, recipient_stream, _) =
        incoming_peer(Ipv4Addr::new(127, 0, 0, 2), [1; 20]).await;
    let (same_ip, same_ip_stream, _) = incoming_peer(Ipv4Addr::new(127, 0, 0, 2), [2; 20]).await;
    let (fresh, fresh_stream, _) = incoming_peer(Ipv4Addr::new(127, 0, 0, 3), [3; 20]).await;
    let (mut stale, stale_stream, _) = incoming_peer(Ipv4Addr::new(127, 0, 0, 4), [4; 20]).await;
    stale.first_contact_time = Instant::now() - PEX_SEND_INTERVAL - Duration::from_secs(1);

    let actors = [
        recipient.actor_id,
        same_ip.actor_id,
        fresh.actor_id,
        stale.actor_id,
    ];
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    for connection in [recipient, same_ip, fresh, stale] {
        assert!(
            swarm
                .spawn_peer(connection, None, Arc::clone(&provider))
                .is_ok()
        );
    }

    let mut recipient_remote = PeerConnection::from_stream_with_peer_capabilities(
        recipient_stream,
        [11; 20],
        false,
        false,
        true,
    );
    let mut same_ip_remote = PeerConnection::from_stream_with_peer_capabilities(
        same_ip_stream,
        [12; 20],
        false,
        false,
        true,
    );
    let mut fresh_remote = PeerConnection::from_stream_with_peer_capabilities(
        fresh_stream,
        [13; 20],
        false,
        false,
        true,
    );
    let mut stale_remote = PeerConnection::from_stream_with_peer_capabilities(
        stale_stream,
        [14; 20],
        false,
        false,
        true,
    );
    for (remote, port) in [
        (&mut recipient_remote, 6880),
        (&mut same_ip_remote, 6881),
        (&mut fresh_remote, 6882),
        (&mut stale_remote, 6883),
    ] {
        send_pex_handshake(remote, Some(port)).await;
    }

    {
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut negotiated = HashSet::new();
            while negotiated.len() < actors.len() {
                if let Some(PeerEvent::ExtensionHandshakeReceived {
                    actor_id,
                    ut_pex_id: Some(19),
                    remote_listen_port: Some(_),
                    ..
                }) = events.recv().await
                {
                    negotiated.insert(actor_id);
                }
            }
        })
        .await
        .expect("all peer actors should negotiate BEP 10 PEX");
    }

    let mut last_pex_send = Instant::now() - PEX_SEND_INTERVAL;
    send_periodic_pex_to_swarm(&mut swarm, &mut last_pex_send, true).await;
    let PexMessage::Added { peers, .. } = read_pex(&mut recipient_remote).await else {
        panic!("expected standard BEP 11 added/dropped message");
    };
    assert_eq!(
        peers
            .iter()
            .map(|peer| peer.addr.port)
            .collect::<HashSet<_>>(),
        HashSet::from([6882]),
        "PEX must exclude the recipient IP and peers older than one minute"
    );

    swarm.shutdown_all().await;
}

#[tokio::test]
async fn abrupt_peer_failure_is_not_recorded_as_dropped_for_pex() {
    let (connection, stream, transport) = incoming_peer(Ipv4Addr::new(127, 0, 0, 5), [5; 20]).await;
    let actor_id = connection.actor_id;
    let advertised = SocketAddr::new(transport.ip(), 6884);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(4);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    let mut remote =
        PeerConnection::from_stream_with_peer_capabilities(stream, [15; 20], false, false, true);
    send_pex_handshake(&mut remote, Some(6884)).await;
    {
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await,
                    Some(PeerEvent::ExtensionHandshakeReceived {
                        actor_id: event_actor,
                        remote_listen_port: Some(6884),
                        ..
                    }) if event_actor == actor_id
                ) {
                    break;
                }
            }
        })
        .await
        .expect("peer actor should apply the advertised listen port");
    }

    remote
        .send_serialized(&[0, 0])
        .await
        .expect("write a deliberately truncated BT frame");
    drop(remote);
    {
        let mut events = swarm.lease_event_receiver().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if matches!(
                    events.recv().await,
                    Some(PeerEvent::Disconnected { actor_id: event_actor })
                        if event_actor == actor_id
                ) {
                    break;
                }
            }
        })
        .await
        .expect("truncated BT frame should disconnect the peer");
    }
    let removed = swarm.remove_dead().await;
    assert!(removed.contains(&(actor_id, transport)));
    assert!(
        !swarm
            .recently_dropped_endpoints()
            .any(|(endpoint, _)| endpoint == advertised)
    );

    swarm.shutdown_all().await;
}
