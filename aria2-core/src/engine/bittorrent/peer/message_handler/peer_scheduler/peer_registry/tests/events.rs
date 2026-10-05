use super::*;

#[tokio::test]
async fn peer_actor_forwards_negotiated_pex_peers_as_a_swarm_event() {
    use aria2_protocol::bittorrent::message::extension::{
        CompactPeerV4, ExtensionHandshake, UtPexMessage,
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer_capabilities(
            local_stream,
            [0; 20],
            false,
            true,
            true,
        ),
        endpoint,
    );
    connection.allocate_session_resource(16, 1, 16);
    connection.set_pex_enabled(true);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    assert!(matches!(
        swarm.spawn_peer(connection, None, provider),
        Ok(registered_actor_id) if registered_actor_id == actor_id
    ));

    let mut remote = PeerConnection::from_stream_with_peer_capabilities(
        remote_stream,
        [1; 20],
        false,
        false,
        true,
    );
    let mut extension_handshake = ExtensionHandshake::new();
    extension_handshake.with_ut_pex(9);
    remote
        .send_message(&BtMessage::Extended {
            ext_id: 0,
            payload: extension_handshake.to_bytes(),
        })
        .await
        .unwrap();
    let negotiation = {
        let mut events = swarm.lease_event_receiver().unwrap();
        timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
    };
    assert!(matches!(
        negotiation,
        PeerEvent::ExtensionHandshakeReceived {
            actor_id: event_actor,
            ut_pex_id: Some(9),
            remote_listen_port: None,
            ..
        } if event_actor == actor_id
    ));
    assert_eq!(swarm.actor(actor_id).unwrap().ut_pex_id, Some(9));

    let mut pex = UtPexMessage::new();
    pex.added.push(CompactPeerV4([127, 0, 0, 1, 0x1a, 0xe1]));
    remote
        .send_message(&BtMessage::Extended {
            ext_id: 9,
            payload: pex.to_payload(),
        })
        .await
        .unwrap();

    let event = {
        let mut events = swarm.lease_event_receiver().unwrap();
        timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
    };
    match event {
        PeerEvent::PexPeers { peers } => {
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].ip, "127.0.0.1");
            assert_eq!(peers[0].port, 6881);
        }
        _ => panic!("expected PEX peer event"),
    }

    swarm.shutdown_all().await;
}

#[tokio::test]
async fn unapplied_event_lease_leaves_registry_updates_to_its_coordinator() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    assert_eq!(
        swarm
            .spawn_peer(connection, None, provider)
            .unwrap_or_else(|_| unreachable!()),
        actor_id
    );
    let old_choke_state = swarm.actor(actor_id).unwrap().stats.peer_choking;
    let event_tx = swarm.event_sender().unwrap();
    let event = PeerEvent::PeerChokingChanged {
        actor_id,
        peer_choking: !old_choke_state,
    };
    event_tx.send(event).await.unwrap();

    let event = {
        let mut lease = swarm.lease_event_receiver().unwrap();
        lease.recv_unapplied().await.unwrap()
    };
    assert_eq!(
        swarm.actor(actor_id).unwrap().stats.peer_choking,
        old_choke_state
    );

    swarm.apply_event(&event);
    assert_eq!(
        swarm.actor(actor_id).unwrap().stats.peer_choking,
        !old_choke_state
    );
    swarm.shutdown_all().await;
    drop(remote_stream);
}

#[tokio::test]
async fn registry_actor_handle_survives_piece_generation_rollover() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.allocate_session_resource(16, 8, 128);
    let actor_id = connection.actor_id;
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut registry = PeerSwarm::new(8);
    registry.insert(PeerActorEntry::spawn(
        actor_id,
        endpoint,
        connection,
        None,
        Some(provider),
        event_tx,
    ));
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    let peer_handle = registry.actor(actor_id).unwrap().handle();
    let first_generation = RequestGeneration::allocate();
    let first_request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };

    peer_handle.begin_generation(first_generation, 2).unwrap();
    peer_handle
        .send(PeerCommand::Request {
            generation: first_generation,
            piece_index: 2,
            request: first_request,
        })
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Request {
            request: PieceBlockRequest::new(2, 0, 16),
        }
    );
    peer_handle.end_generation(first_generation, 2).unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Cancel {
            request: PieceBlockRequest::new(2, 0, 16),
        }
    );
    remote
        .send_message(&BtMessage::Piece {
            index: 2,
            begin: 0,
            data: vec![2; 16].into(),
        })
        .await
        .unwrap();
    let stale_response_deadline = tokio::time::Instant::now() + Duration::from_millis(25);
    while tokio::time::Instant::now() < stale_response_deadline {
        while let Ok(event) = event_rx.try_recv() {
            match event {
                PeerEvent::Message { generation, .. } if generation == first_generation => {
                    panic!("stale block response escaped the peer actor")
                }
                PeerEvent::Disconnected { .. } => {
                    panic!("peer actor disconnected while ignoring a stale block")
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let next_generation = RequestGeneration::allocate();
    peer_handle.begin_generation(next_generation, 5).unwrap();
    peer_handle
        .send(PeerCommand::Request {
            generation: next_generation,
            piece_index: 5,
            request: first_request,
        })
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Request {
            request: PieceBlockRequest::new(5, 0, 16),
        }
    );
    remote
        .send_message(&BtMessage::Piece {
            index: 5,
            begin: 0,
            data: vec![5; 16].into(),
        })
        .await
        .unwrap();
    let event = loop {
        let event = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if !matches!(event, PeerEvent::OutstandingDownloadRequests { .. }) {
            break event;
        }
    };
    assert!(matches!(
        event,
        PeerEvent::Message {
            actor_id: received_actor,
            generation,
            message: BtMessage::Piece { index: 5, .. },
            ..
        } if received_actor == actor_id && generation == next_generation
    ));

    registry.shutdown_all().await;
}

#[tokio::test]
async fn actor_availability_messages_update_swarm_seeder_snapshot_end_to_end() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.allocate_session_resource(16, 8, 128);
    connection.incoming = false;
    connection.source = crate::request::request_group::BtPeerSource::Tracker;
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    assert!(!swarm.actor(actor_id).unwrap().incoming);
    assert!(swarm.actor(actor_id).unwrap().has_bitfield);
    assert_eq!(
        swarm.actor(actor_id).unwrap().source,
        crate::request::request_group::BtPeerSource::Tracker
    );
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

    for (message, expected_seeder) in [
        (BtMessage::Bitfield { data: vec![0xff] }, true),
        (BtMessage::HaveAll, true),
        (BtMessage::HaveNone, false),
    ] {
        remote.send_message(&message).await.unwrap();
        let events = {
            let mut receiver = swarm.lease_event_receiver().unwrap();
            let mut events = Vec::new();
            loop {
                let event = timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let reached_snapshot = matches!(
                    event,
                    PeerEvent::PeerAvailabilitySnapshot { actor_id: event_actor, .. }
                        if event_actor == actor_id
                );
                events.push(event);
                if reached_snapshot {
                    break;
                }
            }
            events
        };
        for event in &events {
            swarm.apply_event(event);
        }

        let actor = swarm.actor(actor_id).unwrap();
        assert_eq!(actor.seeder, expected_seeder);
        assert_eq!(actor.bitfield, if expected_seeder { [0xff] } else { [0] });
    }

    for (message, expected_choking) in [(BtMessage::Unchoke, false), (BtMessage::Choke, true)] {
        remote.send_message(&message).await.unwrap();
        let event = {
            let mut receiver = swarm.lease_event_receiver().unwrap();
            loop {
                let event = timeout(Duration::from_secs(1), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
                if matches!(
                    event,
                    PeerEvent::PeerChokingChanged { actor_id: event_actor, .. }
                        if event_actor == actor_id
                ) {
                    break event;
                }
            }
        };
        swarm.apply_event(&event);
        assert_eq!(
            swarm.actor(actor_id).unwrap().stats.peer_choking,
            expected_choking
        );
    }

    swarm.shutdown_all().await;
}

#[tokio::test]
async fn actor_publishes_seeder_state_when_have_completes_the_bitfield() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.allocate_session_resource(16, 1, 16);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let peer_snapshots = Arc::new(std::sync::RwLock::new(Vec::new()));
    let mut swarm = PeerSwarm::new(8);
    swarm.attach_peer_snapshot_store(Arc::clone(&peer_snapshots));
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    remote
        .send_message(&BtMessage::Have { piece_index: 0 })
        .await
        .unwrap();

    let events = {
        let mut receiver = swarm.lease_event_receiver().unwrap();
        let mut events = Vec::new();
        loop {
            let event = timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("peer must report the seeder-state transition")
                .expect("peer actor event channel must stay open");
            let reached_snapshot = matches!(
                event,
                PeerEvent::PeerAvailabilitySnapshot { actor_id: event_actor, .. }
                    if event_actor == actor_id
            );
            events.push(event);
            if reached_snapshot {
                break;
            }
        }
        events
    };
    for event in &events {
        swarm.apply_event(event);
    }

    let actor = swarm.actor(actor_id).unwrap();
    assert!(actor.seeder);
    assert_eq!(actor.bitfield, [0x80]);
    {
        let snapshots = peer_snapshots
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].seeder, Some(true));
    }

    swarm.shutdown_all().await;
}

#[tokio::test]
async fn registry_applies_peer_availability_and_stats_events_by_actor_id() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.allocate_session_resource(1, 16, 16);
    connection.set_peer_bitfield(&[0x80, 0]);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut registry = PeerSwarm::new(8);
    let event_tx = registry.event_sender().unwrap();
    assert_eq!(
        registry.spawn_peer(connection, None, provider).ok(),
        Some(actor_id)
    );

    event_tx
        .send(PeerEvent::PeerAvailabilityChanged {
            actor_id,
            piece_index: 9,
            has_piece: true,
        })
        .await
        .unwrap();
    let event = registry
        .lease_event_receiver()
        .unwrap()
        .recv()
        .await
        .unwrap();
    registry.apply_event(&event);
    assert_eq!(registry.actor(actor_id).unwrap().bitfield, [0x80, 0x40]);

    let mut snapshot = registry.actor(actor_id).unwrap().stats.clone();
    snapshot.outstanding_upload_count = 2;
    event_tx
        .send(PeerEvent::UploadQueueChanged {
            actor_id,
            snapshot: Box::new(snapshot),
        })
        .await
        .unwrap();
    let event = registry
        .lease_event_receiver()
        .unwrap()
        .recv()
        .await
        .unwrap();
    registry.apply_event(&event);
    assert_eq!(
        registry
            .actor(actor_id)
            .unwrap()
            .stats
            .outstanding_upload_count,
        2
    );

    event_tx
        .send(PeerEvent::PeerAvailabilitySnapshot {
            actor_id,
            bitfield: vec![0x01],
            seeder: true,
        })
        .await
        .unwrap();
    let event = registry
        .lease_event_receiver()
        .unwrap()
        .recv()
        .await
        .unwrap();
    registry.apply_event(&event);
    assert_eq!(registry.actor(actor_id).unwrap().bitfield, [0x01]);
    assert!(registry.actor(actor_id).unwrap().seeder);
    registry.shutdown_all().await;
    drop(remote_stream);
}

#[tokio::test]
async fn swarm_owns_peer_actor_and_routes_bounded_state_events() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.allocate_session_resource(1, 16, 16);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(4);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

    {
        let mut events = swarm.lease_event_receiver().unwrap();
        assert!(
            timeout(Duration::from_millis(1), events.recv())
                .await
                .is_err()
        );
    }

    remote
        .send_message(&BtMessage::Have { piece_index: 3 })
        .await
        .unwrap();
    let availability_event = {
        let mut events = swarm.lease_event_receiver().unwrap();
        timeout(Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap()
    };
    assert!(matches!(
        availability_event,
        PeerEvent::PeerAvailabilityChanged {
            actor_id: event_actor_id,
            piece_index: 3,
            has_piece: true,
        } if event_actor_id == actor_id
    ));
    swarm.apply_event(&availability_event);
    assert_eq!(swarm.actor(actor_id).unwrap().bitfield, [0x10, 0]);
    swarm.shutdown_all().await;
}
