use super::*;

#[tokio::test]
async fn request_failure_keeps_peer_alive_until_transport_disconnect() {
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
        swarm.spawn_peer(connection, None, provider).ok(),
        Some(actor_id)
    );

    swarm.apply_event(&PeerEvent::RequestFailed {
        actor_id,
        generation: RequestGeneration::allocate(),
        piece_index: 0,
        request: BlockRequest {
            block_index: 0,
            offset: 0,
            length: 16,
        },
    });
    assert!(!swarm.actor(actor_id).unwrap().dead);

    swarm.apply_event(&PeerEvent::Disconnected { actor_id });
    assert!(swarm.actor(actor_id).unwrap().dead);
    swarm.shutdown_all().await;
    drop(remote_stream);
}

#[tokio::test]
async fn swarm_registry_preserves_ipv6_peer_endpoints() {
    let listener = tokio::net::TcpListener::bind("[::1]:0").await.unwrap();
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
        swarm.spawn_peer(connection, None, provider).ok(),
        Some(actor_id)
    );

    assert_eq!(swarm.actor(actor_id).unwrap().endpoint, endpoint);

    swarm.shutdown_all().await;
    drop(remote_stream);
}

#[tokio::test]
async fn swarm_coordinator_lease_can_admit_a_peer_without_releasing_event_ownership() {
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
    {
        let mut coordinator = swarm.lease_event_receiver().unwrap();
        assert!(matches!(
            coordinator.spawn_peer(connection, None, provider),
            Ok(registered_actor_id) if registered_actor_id == actor_id
        ));
        assert!(coordinator.actor_mut(actor_id).is_some());
    }
    assert_eq!(swarm.len(), 1);
    assert!(swarm.actor(actor_id).is_some());

    swarm.shutdown_all().await;
    drop(remote_stream);
}

#[tokio::test]
async fn upload_choke_state_updates_peer_wire_state_and_swarm_snapshot() {
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
    let actor_id = connection.actor_id;
    connection.configure_upload_with_auto_unchoke(
        &crate::engine::bittorrent::peer::upload_session::BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        16,
        false,
    );
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    assert!(swarm.set_upload_choked(actor_id, false));
    let mut remote = PeerConnection::from_stream_with_peer_capabilities(
        remote_stream,
        [1; 20],
        false,
        false,
        true,
    );
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Unchoke
    );

    {
        let mut events = swarm.lease_event_receiver().unwrap();
        loop {
            let event = timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("actor should report the upload state transition")
                .expect("swarm event stream should remain open");
            if matches!(event, PeerEvent::ChokeStateChanged { actor_id: id, .. } if id == actor_id)
            {
                break;
            }
        }
    }
    assert!(!swarm.actor(actor_id).unwrap().stats.am_choking);
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn peer_actor_sends_pex_frame_with_the_remote_extension_id() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.register_peer_extension("ut_pex", 19);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    let payload =
        aria2_protocol::bittorrent::extension::pex::PexHandler::build_pex_message(&[], &[])
            .encode();
    let wire = aria2_protocol::bittorrent::message::serializer::serialize_extended(19, payload);
    swarm
        .send_to(actor_id, PeerCommand::SendPex(wire))
        .await
        .unwrap();

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    assert!(matches!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Extended { ext_id: 19, .. }
    ));
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn swarm_broadcasts_validated_piece_have_through_peer_actors() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 4));
    let mut swarm = PeerSwarm::new(8);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    swarm.broadcast_have(3).await;

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Have { piece_index: 3 }
    );
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn swarm_registry_tracks_download_bytes_reported_by_peer_actor() {
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
    let mut swarm = PeerSwarm::new(8);
    assert_eq!(
        swarm.spawn_peer(connection, None, provider).ok(),
        Some(actor_id)
    );

    let generation = RequestGeneration::allocate();
    let request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };
    let control = swarm.actor(actor_id).unwrap().handle();
    control.begin_generation(generation, 0).unwrap();
    control.try_request(generation, 0, request).unwrap();

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Request {
            request: PieceBlockRequest::new(0, 0, 16),
        }
    );
    remote
        .send_message(&BtMessage::Piece {
            index: 0,
            begin: 0,
            data: vec![0x6a; 16].into(),
        })
        .await
        .unwrap();

    let mut event_lease = swarm.lease_event_receiver().unwrap();
    let event = loop {
        let event = timeout(Duration::from_secs(1), event_lease.recv())
            .await
            .unwrap()
            .unwrap();
        if !matches!(event, PeerEvent::OutstandingDownloadRequests { .. }) {
            break event;
        }
    };
    drop(event_lease);
    assert!(matches!(
        event,
        PeerEvent::Message {
            actor_id: event_actor,
            message: BtMessage::Piece { .. },
            stats: Some(_),
            ..
        } if event_actor == actor_id
    ));
    assert_eq!(swarm.actor(actor_id).unwrap().stats.downloaded_bytes, 16);

    swarm.shutdown_all().await;
}

#[tokio::test]
async fn peer_snapshots_use_rolling_rates_instead_of_burst_ema() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let mut connection = BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true),
        endpoint,
    );
    connection.stats.upload_speed = 80_000_000.0;
    connection.stats.download_speed = 120_000_000.0;
    let sample_time = Instant::now() - Duration::from_secs(1);
    connection
        .stats
        .record_upload_rate_at(8 * 1024, sample_time);
    connection
        .stats
        .record_download_rate_at(16 * 1024, sample_time);

    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = PeerSwarm::new(8);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    let snapshot = swarm.peer_snapshots().remove(0);
    assert!(
        (7_000.0..9_000.0).contains(&snapshot.upload_speed),
        "peer snapshot upload speed should use the 10-second byte window, got {}",
        snapshot.upload_speed
    );
    assert!(
        (14_000.0..18_000.0).contains(&snapshot.download_speed),
        "peer snapshot download speed should use the 10-second byte window, got {}",
        snapshot.download_speed
    );

    swarm.shutdown_all().await;
    drop(remote_stream);
}
