use super::*;

#[tokio::test]
async fn peer_actor_sends_startup_allowed_fast_message() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    let mut piece_provider = InMemoryPieceProvider::new(16, 2);
    piece_provider.set_piece_data(0, vec![0xA5; 16]);
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        2,
        16,
        false,
    );
    connection.actor_startup = Some(
        crate::engine::bittorrent::peer::connection::PeerActorStartup {
            peer_agent: "test-peer".to_string(),
            listen_port: None,
            allowed_fast: vec![3],
        },
    );

    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let provider = Arc::new(piece_provider);
    let worker = tokio::spawn(async move {
        run_peer_actor(
            actor_id,
            &mut connection,
            command_rx,
            event_tx,
            None,
            Some(provider),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
        .await;
    });

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::Bitfield {
            data: vec![0b1000_0000]
        },
        "outbound actor startup must advertise only verified local pieces"
    );
    let allowed_fast = timeout(Duration::from_secs(1), remote.read_message()).await;
    assert!(
        allowed_fast.is_ok(),
        "peer actor stopped during startup: {}",
        matches!(event_rx.try_recv(), Ok(PeerEvent::Disconnected { actor_id: id }) if id == actor_id)
    );
    assert_eq!(
        allowed_fast.unwrap().unwrap().unwrap(),
        BtMessage::AllowedFast { index: 3 }
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn peer_actor_advertises_dht_udp_port_not_tcp_listen_port() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], true, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        16,
        false,
    );
    connection.actor_startup = Some(
        crate::engine::bittorrent::peer::connection::PeerActorStartup {
            peer_agent: "test-peer".to_string(),
            listen_port: Some(6881),
            allowed_fast: vec![0],
        },
    );
    let dht = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
        aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
    )
    .await
    .expect("local DHT engine should start");
    let dht_port = dht.local_addr().port();

    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel(8);
    let (event_tx, _event_rx) = mpsc::channel(8);
    let provider = Arc::new(InMemoryPieceProvider::new(16, 1));
    let actor_dht = Arc::clone(&dht);
    let worker = tokio::spawn(async move {
        let mut connection = connection;
        run_peer_actor(
            actor_id,
            &mut connection,
            command_rx,
            event_tx,
            Some(actor_dht),
            Some(provider),
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
        .await;
    });

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::HaveNone,
        "availability must precede DHT and AllowedFast startup messages"
    );
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::Port { port: dht_port },
        "BEP 5 PORT must advertise the selected DHT engine's UDP port before AllowedFast"
    );
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::AllowedFast { index: 0 }
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
    dht.shutdown_async().await;
}

#[tokio::test]
async fn peer_actor_publishes_piece_availability_independently_of_request_generation() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.allocate_session_resource(16 * 1024, 8, 8 * 16 * 1024);
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let worker = tokio::spawn(async move {
        run_peer_actor(
            actor_id,
            &mut connection,
            command_rx,
            event_tx,
            None,
            None,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
        .await;
        connection
    });
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    remote
        .send_message(&BtMessage::Have { piece_index: 6 })
        .await
        .unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        PeerEvent::PeerAvailabilityChanged {
            actor_id: event_actor_id,
            piece_index: 6,
            has_piece: true,
        } if event_actor_id == actor_id
    ));

    let generation = RequestGeneration::allocate();
    command_tx.begin_generation(generation, 7).unwrap();
    remote
        .send_message(&BtMessage::Have { piece_index: 7 })
        .await
        .unwrap();

    assert!(matches!(
        timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        PeerEvent::PeerAvailabilityChanged {
            actor_id: event_actor_id,
            piece_index: 7,
            has_piece: true,
        } if event_actor_id == actor_id
    ));
    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn peer_actor_publishes_full_availability_outside_generation() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.allocate_session_resource(16 * 1024, 8, 8 * 16 * 1024);
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let worker = tokio::spawn(async move {
        run_peer_actor(
            actor_id,
            &mut connection,
            command_rx,
            event_tx,
            None,
            None,
            Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        )
        .await;
    });
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);

    for (message, expected) in [
        (
            BtMessage::Bitfield {
                data: vec![0b1010_0000],
            },
            vec![0b1010_0000],
        ),
        (BtMessage::HaveAll, vec![0xff]),
        (BtMessage::HaveNone, vec![0]),
    ] {
        remote.send_message(&message).await.unwrap();
        assert!(matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            PeerEvent::PeerAvailabilitySnapshot {
                actor_id: event_actor_id,
                bitfield,
                seeder,
            } if event_actor_id == actor_id
                && bitfield == expected
                && seeder == matches!(message, BtMessage::HaveAll)
        ));
    }

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn generation_finish_preserves_queued_availability_events() {
    let actor_id = PeerActorId(41);
    let mut swarm = PeerSwarm::new(2);
    swarm
        .event_tx
        .as_ref()
        .unwrap()
        .send(PeerEvent::PeerAvailabilityChanged {
            actor_id,
            piece_index: 2,
            has_piece: true,
        })
        .await
        .unwrap();
    let mut workers = PeerGeneration {
        senders: HashMap::new(),
        generation: RequestGeneration::allocate(),
        active_pieces: HashSet::from([2]),
        availability_changed_actor_ids: HashSet::new(),
        pex_peers: Vec::new(),
    };

    let mut event_stream = swarm.lease_event_receiver().unwrap();
    workers.finish_generation(&mut event_stream).await;

    assert!(workers.take_availability_changes().contains(&actor_id));
}
