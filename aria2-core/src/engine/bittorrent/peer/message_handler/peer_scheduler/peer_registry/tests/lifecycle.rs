use super::*;

#[tokio::test]
async fn shutdown_all_closes_a_full_event_queue_before_joining_peer_actors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    let mut piece_provider = InMemoryPieceProvider::new(16, 1);
    piece_provider.set_piece_data(0, vec![0x5a; 16]);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(piece_provider);
    connection.configure_upload_with_auto_unchoke(
        &crate::engine::bittorrent::peer::upload_session::BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        16,
        true,
    );
    connection.choke_upload_peer().await.unwrap();
    connection.unchoke_upload_peer().await.unwrap();

    let actor_id = connection.actor_id;
    let mut swarm = PeerSwarm::new(1);
    assert!(matches!(
        swarm.spawn_peer(connection, None, Arc::clone(&provider)),
        Ok(registered_actor_id) if registered_actor_id == actor_id
    ));
    let event_sender = swarm.event_sender().unwrap();
    event_sender
        .send(PeerEvent::PeerChokingChanged {
            actor_id,
            peer_choking: true,
        })
        .await
        .unwrap();

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Choke
    );
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Unchoke
    );
    remote.send_message(&BtMessage::Interested).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Unchoke
    );

    timeout(Duration::from_secs(1), swarm.shutdown_all())
        .await
        .expect("swarm shutdown must not wait on a full peer event queue");
    assert!(swarm.is_empty());
    assert!(swarm.event_sender().is_none());
    drop(event_sender);
}

#[tokio::test]
async fn removing_dead_actor_cancels_inflight_request_before_joining() {
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
    let (event_tx, _event_rx) = mpsc::channel(8);
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
    let generation = RequestGeneration::allocate();
    let request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };

    peer_handle.begin_generation(generation, 3).unwrap();
    peer_handle
        .send(PeerCommand::Request {
            generation,
            piece_index: 3,
            request,
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
            request: PieceBlockRequest::new(3, 0, 16),
        }
    );

    registry.mark_dead(actor_id);
    assert_eq!(
        timeout(Duration::from_secs(1), registry.remove_dead())
            .await
            .unwrap(),
        vec![(actor_id, endpoint)]
    );
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Cancel {
            request: PieceBlockRequest::new(3, 0, 16),
        }
    );
    assert!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert!(registry.is_empty());
}

#[tokio::test]
async fn cancelled_actor_shutdown_keeps_join_handle_for_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    let actor_id = connection.actor_id;
    let (event_tx, mut event_rx) = mpsc::channel(1);
    let mut actor = PeerActorTask::spawn_owned(
        actor_id,
        connection,
        event_tx.clone(),
        None,
        None,
        Arc::new(AtomicUsize::new(0)),
        4,
    );
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

    event_tx
        .send(PeerEvent::PeerAvailabilityChanged {
            actor_id,
            piece_index: 0,
            has_piece: false,
        })
        .await
        .unwrap();
    remote
        .send_message(&BtMessage::Have { piece_index: 0 })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    {
        let shutdown = actor.shutdown();
        tokio::pin!(shutdown);
        assert!(
            timeout(Duration::from_millis(25), &mut shutdown)
                .await
                .is_err()
        );
    }

    assert!(matches!(
        event_rx.recv().await,
        Some(PeerEvent::PeerAvailabilityChanged { .. })
    ));
    assert!(matches!(
        timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap(),
        Some(PeerEvent::PeerAvailabilityChanged { .. })
    ));
    timeout(Duration::from_secs(1), actor.shutdown())
        .await
        .unwrap()
        .unwrap();
    timeout(Duration::from_secs(1), actor.shutdown())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn removing_multiple_dead_actors_rebuilds_stable_id_index() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut registry = PeerSwarm::new(8);
    let mut endpoints = Vec::new();
    let mut actor_ids = Vec::new();
    let mut remote_streams = Vec::new();

    for _ in 0..3 {
        let remote_stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (local_stream, endpoint) = listener.accept().await.unwrap();
        let connection = BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
            endpoint,
        );
        actor_ids.push(connection.actor_id);
        endpoints.push(endpoint);
        registry.insert(PeerActorEntry::spawn(
            connection.actor_id,
            endpoint,
            connection,
            None,
            Some(Arc::clone(&provider)),
            event_tx.clone(),
        ));
        remote_streams.push(remote_stream);
    }

    registry.mark_dead(actor_ids[0]);
    registry.mark_dead(actor_ids[2]);
    assert_eq!(
        timeout(Duration::from_secs(1), registry.remove_dead())
            .await
            .unwrap(),
        vec![(actor_ids[2], endpoints[2]), (actor_ids[0], endpoints[0])]
    );
    assert_eq!(registry.len(), 1);
    assert_eq!(
        registry.actor(actor_ids[1]).map(|actor| actor.actor_id),
        Some(actor_ids[1])
    );
    assert!(registry.actor(actor_ids[0]).is_none());
    assert!(registry.actor(actor_ids[2]).is_none());

    registry.shutdown_all().await;
    drop(remote_streams);
}
