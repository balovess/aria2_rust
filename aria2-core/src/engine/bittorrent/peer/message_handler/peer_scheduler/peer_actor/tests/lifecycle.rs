use super::*;

#[tokio::test]
async fn peer_actor_cancels_inflight_request_before_orderly_shutdown() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel(8);
    let (event_tx, _event_rx) = mpsc::channel(8);
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
    let generation = RequestGeneration::allocate();
    let request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16 * 1024,
    };
    command_tx.begin_generation(generation, 3).unwrap();
    command_tx
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
            request: PieceBlockRequest::new(3, 0, 16 * 1024),
        }
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Cancel {
            request: PieceBlockRequest::new(3, 0, 16 * 1024),
        }
    );
    worker.await.unwrap();
}

#[tokio::test]
async fn peer_generation_reuses_actor_io_across_retry() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let remote_stream = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.allocate_session_resource(16, 8, 128);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 8));
    let mut swarm = PeerSwarm::new(16);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    let mut workers = PeerGeneration::from_swarm(&swarm, &[2]);
    let mut event_rx = swarm.lease_event_receiver().unwrap();
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    let first_generation = workers.generation();
    let first_request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };

    workers
        .senders
        .get(&actor_id)
        .as_ref()
        .unwrap()
        .try_request(first_generation, 2, first_request)
        .unwrap();
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::Request {
            request: PieceBlockRequest::new(2, 0, 16),
        }
    );

    workers.advance_generations();
    let retry_generation = workers.generation();
    assert_ne!(first_generation, retry_generation);
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::Cancel {
            request: PieceBlockRequest::new(2, 0, 16),
        }
    );
    remote
        .send_message(&BtMessage::Piece {
            index: 2,
            begin: 0,
            data: vec![0x22; 16].into(),
        })
        .await
        .unwrap();
    workers
        .senders
        .get(&actor_id)
        .as_ref()
        .unwrap()
        .try_request(retry_generation, 2, first_request)
        .unwrap();
    assert_eq!(
        read_message_while_actor_runs(&mut remote).await,
        BtMessage::Request {
            request: PieceBlockRequest::new(2, 0, 16),
        }
    );
    remote
        .send_message(&BtMessage::Piece {
            index: 2,
            begin: 0,
            data: vec![0x55; 16].into(),
        })
        .await
        .unwrap();
    assert!(matches!(
        receive_event_while_actor_runs(&mut event_rx).await,
        PeerEvent::Message {
            actor_id: event_actor_id,
            generation,
            message: BtMessage::Piece { index: 2, .. },
            ..
        } if event_actor_id == actor_id && generation == retry_generation
    ));

    workers.finish_generation(&mut event_rx).await;
    drop(event_rx);
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn explicit_peer_actor_shutdown_cancels_inflight_request() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    let actor_id = connection.actor_id;
    let (event_tx, _event_rx) = mpsc::channel(8);
    let mut actor = PeerActorTask::spawn_owned(
        actor_id,
        connection,
        event_tx,
        None,
        None,
        Arc::new(AtomicUsize::new(0)),
        8,
    );
    let control = actor.control.clone();
    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    let generation = RequestGeneration::allocate();
    let request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16 * 1024,
    };
    control.begin_generation(generation, 5).unwrap();
    control
        .send(PeerCommand::Request {
            generation,
            piece_index: 5,
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
            request: PieceBlockRequest::new(5, 0, 16 * 1024),
        }
    );

    let shutdown = tokio::spawn(async move {
        let first = actor.shutdown().await;
        let second = actor.shutdown().await;
        (first, second)
    });

    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Cancel {
            request: PieceBlockRequest::new(5, 0, 16 * 1024),
        }
    );
    let (first, second) = shutdown.await.unwrap();
    first.unwrap();
    second.unwrap();
}

#[tokio::test]
async fn peer_actor_shutdown_is_bounded_when_io_never_completes() {
    let (control, _receiver) = PeerActorControl::channel(1);
    let task = tokio::spawn(std::future::pending::<()>());
    let mut actor = PeerActorTask {
        control,
        task: Some(task),
    };
    let started = Instant::now();

    tokio::time::timeout(Duration::from_secs(2), actor.shutdown())
        .await
        .expect("peer actor shutdown must be bounded")
        .expect("aborting a stalled peer actor is successful cleanup");

    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(actor.task.is_none());
}

#[tokio::test]
async fn cancelling_peer_actor_shutdown_retains_join_handle_for_retry() {
    let (control, _receiver) = PeerActorControl::channel(1);
    let task = tokio::spawn(std::future::pending::<()>());
    let mut actor = PeerActorTask {
        control,
        task: Some(task),
    };

    {
        let shutdown = actor.shutdown();
        tokio::pin!(shutdown);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut shutdown)
                .await
                .is_err()
        );
    }

    assert!(
        actor.task.is_some(),
        "cancelling the shutdown future must not detach the peer task"
    );
    actor.task.as_ref().unwrap().abort();
    actor
        .shutdown()
        .await
        .expect("a retried shutdown must join the aborted task");
    assert!(actor.task.is_none());
}

#[tokio::test]
async fn peer_actor_survives_piece_generation_rollover_and_drops_late_blocks() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel(8);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let worker = tokio::spawn(async move {
        let mut connection = connection;
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
    let first_generation = RequestGeneration::allocate();
    let first_request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };
    command_tx.begin_generation(first_generation, 3).unwrap();
    command_tx
        .send(PeerCommand::Request {
            generation: first_generation,
            piece_index: 3,
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
            request: PieceBlockRequest::new(3, 0, 16),
        }
    );

    command_tx.end_generation(first_generation, 3).unwrap();
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
    let request_count_event = loop {
        let event = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(
            event,
            PeerEvent::OutstandingDownloadRequests { count: 0, .. }
        ) {
            break event;
        }
    };
    assert!(matches!(
        request_count_event,
        PeerEvent::OutstandingDownloadRequests { actor_id: event_actor, count: 0 }
            if event_actor == actor_id
    ));
    remote
        .send_message(&BtMessage::Piece {
            index: 3,
            begin: 0,
            data: vec![0x33; 16].into(),
        })
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(50), event_rx.recv())
            .await
            .is_err()
    );

    let next_generation = RequestGeneration::allocate();
    let next_request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };
    command_tx.begin_generation(next_generation, 4).unwrap();
    command_tx
        .send(PeerCommand::Request {
            generation: next_generation,
            piece_index: 4,
            request: next_request,
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
            request: PieceBlockRequest::new(4, 0, 16),
        }
    );
    remote
        .send_message(&BtMessage::Piece {
            index: 4,
            begin: 0,
            data: vec![0x44; 16].into(),
        })
        .await
        .unwrap();
    let piece_event = loop {
        let event = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if !matches!(event, PeerEvent::OutstandingDownloadRequests { .. }) {
            break event;
        }
    };
    assert!(matches!(
        piece_event,
        PeerEvent::Message {
            actor_id: received_actor,
            generation,
            message: BtMessage::Piece { index: 4, .. },
            ..
        } if received_actor == actor_id && generation == next_generation
    ));

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    timeout(Duration::from_secs(1), worker)
        .await
        .unwrap()
        .unwrap();
}
