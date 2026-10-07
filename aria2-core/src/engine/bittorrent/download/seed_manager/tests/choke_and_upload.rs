use super::*;

#[tokio::test]
async fn seeding_choke_decision_runs_when_interest_and_choke_state_mismatch() {
    let (mut manager, _client) = manager_with_dead_seed_peer(true, false).await;
    manager.config.max_peers_to_unchoke = 4;
    let actor_id = manager.swarm.iter().next().unwrap().actor_id;
    manager.swarm.actor_mut(actor_id).unwrap().dead = false;
    // Interested + choked needs an unchoke decision.
    {
        let stats = &mut manager.swarm.actor_mut(actor_id).unwrap().stats;
        stats.peer_interested = true;
        stats.am_choking = true;
    }
    assert!(
        manager
            .swarm
            .actor_mut(actor_id)
            .unwrap()
            .handle()
            .set_upload_choked(true)
    );
    assert!(manager.any_peer_choke_state_mismatch());

    // Not interested + unchoked needs a choke decision immediately.
    {
        let stats = &mut manager.swarm.actor_mut(actor_id).unwrap().stats;
        stats.peer_interested = false;
        stats.am_choking = false;
    }
    assert!(
        manager
            .swarm
            .actor_mut(actor_id)
            .unwrap()
            .handle()
            .set_upload_choked(false)
    );
    assert!(manager.any_peer_choke_state_mismatch());

    // Both settled states do not trigger an unnecessary choke round.
    {
        let actor = manager.swarm.actor_mut(actor_id).unwrap();
        actor.stats.am_choking = true;
        assert!(actor.handle().set_upload_choked(true));
    }
    assert!(!manager.any_peer_choke_state_mismatch());
    {
        let stats = &mut manager.swarm.actor_mut(actor_id).unwrap().stats;
        stats.peer_interested = true;
        stats.am_choking = false;
    }
    assert!(
        manager
            .swarm
            .actor_mut(actor_id)
            .unwrap()
            .handle()
            .set_upload_choked(false)
    );
    assert!(!manager.any_peer_choke_state_mismatch());

    manager.config.max_peers_to_unchoke = 0;
    manager.swarm.actor_mut(actor_id).unwrap().stats.am_choking = true;
    assert!(
        manager
            .swarm
            .actor_mut(actor_id)
            .unwrap()
            .handle()
            .set_upload_choked(true)
    );
    assert!(
        !manager.any_peer_choke_state_mismatch(),
        "a choked interested peer is settled when the configured upload-slot capacity is zero"
    );
}

#[tokio::test]
async fn seeding_manager_adopts_the_existing_torrent_peer_actor() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server, endpoint) = listener.accept().await.unwrap();
    let mut connection = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(server, [2u8; 20], false, false),
        endpoint,
    );
    connection.allocate_session_resource(16, 1, 16);
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        16,
        false,
    );
    connection.actor_startup = Some(
        crate::engine::bittorrent::peer::connection::PeerActorStartup {
            peer_agent: aria2_protocol::identity::DEFAULT_PEER_AGENT.to_string(),
            listen_port: None,
            allowed_fast: Vec::new(),
        },
    );
    connection.stats.am_choking = true;
    let actor_id = connection.actor_id;
    let mut swarm = PeerSwarm::new(8);
    let mut provider =
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1);
    provider.set_piece_data(0, vec![0x5a; 16]);
    let provider = Arc::new(provider);
    let provider_dyn: Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider> =
        provider;
    assert!(
        swarm
            .spawn_peer(connection, None, Arc::clone(&provider_dyn))
            .is_ok()
    );
    let upload_counter = Arc::new(AtomicU64::new(0));

    let mut manager = BtSeedManager::new_with_swarm(
        [7u8; 20],
        swarm,
        provider_dyn,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        None,
        [1u8; 20],
        None,
        upload_counter,
        std::time::Instant::now(),
    );
    assert!(manager.swarm.actor(actor_id).is_some());
    manager.cancel();
    manager.run_seeding_loop().await.unwrap();

    let mut client =
        PeerConnection::from_stream_with_peer(client_task.await.unwrap(), [1u8; 20], false, false);
    assert!(matches!(
        tokio::time::timeout(std::time::Duration::from_secs(1), client.read_message())
            .await
            .unwrap()
            .unwrap(),
        Some(aria2_protocol::bittorrent::message::types::BtMessage::Bitfield { .. })
    ));
    assert!(manager.swarm.is_empty());
}

#[tokio::test]
async fn seeding_manager_actorizes_pending_connections_when_swarm_is_already_populated() {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1),
    );
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider.clone(),
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        None,
        [1u8; 20],
        None,
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let existing_client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (existing_stream, existing_endpoint) = listener.accept().await.unwrap();
    let existing = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(existing_stream, [2u8; 20], false, false),
        existing_endpoint,
    );
    assert!(
        manager
            .swarm
            .spawn_peer(existing, None, provider.clone())
            .is_ok()
    );

    let pending_client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (pending_stream, pending_endpoint) = listener.accept().await.unwrap();
    manager.pending_connections.push(
        crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(pending_stream, [3u8; 20], false, false),
            pending_endpoint,
        ),
    );
    assert_eq!(manager.num_sessions(), 2);

    manager.cancel();
    manager.run_seeding_loop().await.unwrap();

    assert!(manager.pending_connections.is_empty());
    assert_eq!(manager.num_sessions(), 0);
    drop(existing_client.await.unwrap());
    drop(pending_client.await.unwrap());
}

async fn manager_with_dead_seed_peer(
    am_choking: bool,
    peer_interested: bool,
) -> (BtSeedManager, TcpStream) {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider.clone(),
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        1024,
        None,
        [1u8; 20],
        None,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server, endpoint) = listener.accept().await.unwrap();
    let mut connection = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(server, [2u8; 20], false, false),
        endpoint,
    );
    let actor_id = connection.actor_id;
    connection.stats.am_choking = am_choking;
    connection.stats.peer_interested = peer_interested;

    assert!(manager.swarm.spawn_peer(connection, None, provider).is_ok());
    manager.swarm.actor_mut(actor_id).unwrap().dead = true;

    (manager, client.await.unwrap())
}

#[tokio::test]
async fn seed_manager_keeps_recent_upload_speed_for_the_full_rate_window() {
    let (mut manager, client) = manager_with_dead_seed_peer(true, false).await;
    let now = Instant::now();
    let sample_time = now - Duration::from_secs(5);
    let actor = manager.swarm.iter_mut().next().expect("seed peer actor");
    let actor_id = actor.actor_id;
    let mut snapshot = actor.stats.clone();
    snapshot.uploaded_bytes = 4096;
    snapshot.record_upload_rate_at(4096, sample_time);
    snapshot.last_upload_time = Some(sample_time);
    manager.swarm.apply_event(&PeerEvent::UploadBytes {
        actor_id,
        bytes: 4096,
        recorded_at: sample_time,
        snapshot: Box::new(snapshot),
    });

    assert!(
        manager.current_upload_speed() > 0,
        "recent payload remains in aria2's 10-second upload rate window"
    );

    manager.swarm.shutdown_all().await;
    drop(client);
}

#[tokio::test]
async fn peer_registry_reindexes_surviving_actor_after_dead_peer_removal() {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider.clone(),
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        1024,
        None,
        [1u8; 20],
        None,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first_client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let second_client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (first_stream, first_endpoint) = listener.accept().await.unwrap();
    let (second_stream, second_endpoint) = listener.accept().await.unwrap();
    let first_connection =
        crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(first_stream, [2u8; 20], false, false),
            first_endpoint,
        );
    let second_connection =
        crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(second_stream, [3u8; 20], false, false),
            second_endpoint,
        );
    let first_id = first_connection.actor_id;
    let second_id = second_connection.actor_id;
    assert!(
        manager
            .swarm
            .spawn_peer(first_connection, None, provider.clone())
            .is_ok()
    );
    assert!(
        manager
            .swarm
            .spawn_peer(second_connection, None, provider)
            .is_ok()
    );
    assert!(manager.swarm.has_peer_id([2u8; 20]));
    assert!(manager.swarm.has_peer_id([3u8; 20]));
    assert!(manager.swarm.has_endpoint(first_endpoint));
    assert!(manager.swarm.has_endpoint(second_endpoint));
    manager.swarm.mark_dead(first_id);

    assert!(!manager.remove_dead_sessions().await);
    assert!(manager.swarm.actor(first_id).is_none());
    assert_eq!(manager.swarm.len(), 1);
    assert!(manager.swarm.actor(second_id).is_some());
    assert!(!manager.swarm.has_peer_id([2u8; 20]));
    assert!(manager.swarm.has_peer_id([3u8; 20]));
    assert!(!manager.swarm.has_endpoint(first_endpoint));
    assert!(manager.swarm.has_endpoint(second_endpoint));
    assert_eq!(
        manager.swarm.actor(second_id).unwrap().stats.peer_id,
        [3u8; 20]
    );

    manager
        .swarm
        .send_to(second_id, PeerCommand::Shutdown)
        .await
        .unwrap();
    let mut surviving_client = second_client.await.unwrap();
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            surviving_client.read_u8()
        )
        .await
        .unwrap()
        .is_err()
    );

    drop(first_client.await.unwrap());
    drop(surviving_client);
}

#[tokio::test]
async fn incoming_peer_with_existing_swarm_identity_is_rejected() {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
    let (incoming_sender, incoming_receiver) = mpsc::channel(1);
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider.clone(),
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        1024,
        None,
        [1u8; 20],
        Some(incoming_receiver),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    let existing_client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (existing_stream, existing_endpoint) = listener.accept().await.unwrap();
    let existing = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(existing_stream, [2u8; 20], false, false),
        existing_endpoint,
    );
    assert!(
        manager
            .swarm
            .spawn_peer(existing, None, provider.clone())
            .is_ok()
    );

    let incoming_client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (incoming_stream, incoming_endpoint) = listener.accept().await.unwrap();
    let incoming = PeerConnection::from_stream_with_peer(incoming_stream, [2u8; 20], false, false);
    incoming_sender
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection: incoming,
            endpoint: incoming_endpoint,
        })
        .await
        .unwrap();
    manager.drain_incoming_peers().await;

    assert_eq!(manager.swarm.len(), 1);
    manager.swarm.shutdown_all().await;
    drop(existing_client.await.unwrap());
    drop(incoming_client.await.unwrap());
}

#[tokio::test]
async fn removing_unchoked_interested_seed_peer_requests_immediate_choke_round() {
    let (mut manager, _client) = manager_with_dead_seed_peer(false, true).await;

    let needs_choke_round = manager.remove_dead_sessions().await;

    assert!(needs_choke_round);
    assert!(manager.swarm.is_empty());
}

#[tokio::test]
async fn removing_settled_seed_peer_does_not_force_early_choke_round() {
    let (mut manager, _client) = manager_with_dead_seed_peer(true, false).await;

    assert!(!manager.remove_dead_sessions().await);
    assert!(manager.swarm.is_empty());
}

#[tokio::test]
async fn seed_manager_applies_outstanding_upload_queue_snapshot() {
    let (mut manager, _client) = manager_with_dead_seed_peer(true, false).await;
    let actor_id = manager.swarm.iter().next().unwrap().actor_id;
    {
        let actor = manager.swarm.actor_mut(actor_id).unwrap();
        actor.seeder = true;
        actor.incoming = false;
        actor.source = crate::request::request_group::BtPeerSource::Tracker;
        actor.has_bitfield = true;
        actor.bitfield = vec![0xa0, 0x40];
    }
    let mut snapshot = manager.swarm.actor(actor_id).unwrap().stats.clone();
    snapshot.outstanding_upload_count = 3;
    snapshot.downloaded_bytes = 47;
    snapshot.upload_speed = 80_000_000.0;
    snapshot.download_speed = 120_000_000.0;
    snapshot.avg_download_speed = 7;
    let sample_time = Instant::now() - std::time::Duration::from_secs(1);
    snapshot.record_upload_rate_at(8 * 1024, sample_time);
    snapshot.record_download_rate_at(16 * 1024, sample_time);

    manager.apply_peer_event(
        crate::engine::bittorrent::peer::message_handler::PeerEvent::UploadQueueChanged {
            actor_id,
            snapshot: Box::new(snapshot),
        },
    );

    assert_eq!(
        manager
            .swarm
            .actor(actor_id)
            .unwrap()
            .stats
            .outstanding_upload_count,
        3
    );
    let peer_snapshot = manager.peer_snapshots().remove(0);
    assert_eq!(peer_snapshot.seeder, Some(true));
    assert!(!peer_snapshot.is_incoming);
    assert_eq!(
        peer_snapshot.source,
        crate::request::request_group::BtPeerSource::Tracker
    );
    assert_eq!(peer_snapshot.bitfield, Some(vec![0xa0, 0x40]));
    assert_eq!(peer_snapshot.downloaded_bytes, 47);
    assert!((7_000.0..9_000.0).contains(&peer_snapshot.upload_speed));
    assert!((14_000.0..18_000.0).contains(&peer_snapshot.download_speed));
    assert_eq!(peer_snapshot.avg_download_speed, 7);
    manager.apply_peer_event(
        crate::engine::bittorrent::peer::message_handler::PeerEvent::PeerChokingChanged {
            actor_id,
            peer_choking: false,
        },
    );
    assert!(!manager.peer_snapshots()[0].peer_choking);
    manager.apply_peer_event(
        crate::engine::bittorrent::peer::message_handler::PeerEvent::PeerChokingChanged {
            actor_id,
            peer_choking: true,
        },
    );
    assert!(manager.peer_snapshots()[0].peer_choking);
}
