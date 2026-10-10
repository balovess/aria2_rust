use super::*;

#[tokio::test]
async fn active_peer_actor_cancels_queued_upload_piece_before_flush() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        32,
        true,
    );

    let mut provider = InMemoryPieceProvider::new(32, 1);
    provider.set_piece_data(0, vec![0x71; 32]);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel_with_initial_choke(8, false);
    let (event_tx, mut event_rx) = mpsc::channel(8);
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
        connection
    });

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    let request = PieceBlockRequest::new(0, 8, 8);
    remote
        .send_message(&BtMessage::Request {
            request: request.clone(),
        })
        .await
        .unwrap();
    let first_event = timeout(Duration::from_secs(1), event_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let event_kind = if matches!(&first_event, PeerEvent::ChokeStateChanged { .. }) {
        "ChokeStateChanged"
    } else {
        "another PeerEvent variant"
    };
    assert!(
        matches!(
            &first_event,
            PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                if *event_actor_id == actor_id && snapshot.outstanding_upload_count == 1
        ),
        "unexpected first peer event: {event_kind}"
    );
    remote
        .send_message(&BtMessage::Cancel { request })
        .await
        .unwrap();
    assert!(matches!(
        timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap(),
        PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
            if event_actor_id == actor_id && snapshot.outstanding_upload_count == 0
    ));

    let response = timeout(Duration::from_secs(1), remote.read_message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        response,
        BtMessage::Reject {
            index: 0,
            offset: 8,
            length: 8,
        }
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    let _connection = worker.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn upload_rate_change_wakes_actor_without_retry_polling() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    let limiter = crate::rate_limiter::RateLimiter::new(
        &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
    );
    let global_limiter = crate::rate_limiter::RateLimiter::new(
        &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
    );
    let config = BtSeedingConfig {
        global_limiter: Some(global_limiter.clone()),
        ..BtSeedingConfig::default()
    };
    connection.configure_upload_with_auto_unchoke(&config, limiter.clone(), 1, 16, true);

    let mut provider = InMemoryPieceProvider::new(16, 1);
    provider.set_piece_data(0, vec![0x71; 16]);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel_with_initial_choke(8, false);
    let (event_tx, mut event_rx) = mpsc::channel(8);
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
    remote.send_message(&BtMessage::Interested).await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        BtMessage::Unchoke
    );
    remote
        .send_message(&BtMessage::Request {
            request: PieceBlockRequest::new(0, 0, 8),
        })
        .await
        .unwrap();
    tokio::task::yield_now().await;

    loop {
        let event = timeout(Duration::from_secs(5), event_rx.recv())
            .await
            .unwrap()
            .expect("peer actor event channel closed");
        if matches!(
            event,
            PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                if event_actor_id == actor_id && snapshot.outstanding_upload_count == 1
        ) {
            break;
        }
    }

    assert!(
        timeout(Duration::from_millis(80), remote.read_message())
            .await
            .is_err(),
        "the request must remain queued while the one-byte-per-second limiter has no tokens"
    );
    limiter.set_upload_rate(None);
    global_limiter.set_upload_rate(None);

    assert_eq!(
        timeout(Duration::from_millis(100), remote.read_message())
            .await
            .expect("rate change should wake the actor without waiting for its old retry deadline")
            .unwrap()
            .unwrap(),
        BtMessage::Piece {
            index: 0,
            begin: 0,
            data: vec![0x71; 8].into(),
        }
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn upload_rate_change_wakes_actor_after_magnet_payload_activation() {
    use tokio::time::{Duration, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.enter_metadata_mode();

    let limiter = crate::rate_limiter::RateLimiter::new(
        &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
    );
    let global_limiter = crate::rate_limiter::RateLimiter::new(
        &crate::rate_limiter::RateLimiterConfig::new(None, Some(1)).with_burst(None, Some(0)),
    );
    let actor_id = connection.actor_id;
    let (command_tx, command_rx) = PeerActorControl::channel_with_initial_choke(8, false);
    let (event_tx, mut event_rx) = mpsc::channel(8);
    let worker = tokio::spawn(async move {
        run_peer_actor(
            actor_id,
            &mut connection,
            command_rx,
            event_tx,
            None,
            None,
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
    });

    let mut provider = InMemoryPieceProvider::new(16, 1);
    provider.set_piece_data(0, vec![0x71; 16]);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
    command_tx
        .send(PeerCommand::ActivatePayload(Arc::new(
            PeerActorPayloadConfig {
                network_info_hash: [0; 20],
                local_metadata: Arc::from([]),
                piece_length: 16,
                num_pieces: 1,
                total_length: 16,
                upload_config: BtSeedingConfig {
                    global_limiter: Some(global_limiter.clone()),
                    ..BtSeedingConfig::default()
                },
                upload_limiter: limiter.clone(),
                auto_unchoke: true,
                upload_counter: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                upload_progress: Arc::new(crate::request::request_group::AtomicProgress::new()),
                provider: Arc::clone(&provider),
            },
        )))
        .await
        .unwrap();

    let mut remote = PeerConnection::from_stream_with_peer(remote_stream, [1; 20], false, false);
    remote.send_message(&BtMessage::Interested).await.unwrap();
    loop {
        let message = timeout(Duration::from_secs(1), remote.read_message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if message == BtMessage::Unchoke {
            break;
        }
    }
    remote
        .send_message(&BtMessage::Request {
            request: PieceBlockRequest::new(0, 0, 8),
        })
        .await
        .unwrap();

    loop {
        let event = timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .expect("peer actor event channel closed");
        if matches!(
            event,
            PeerEvent::UploadQueueChanged { actor_id: event_actor_id, snapshot }
                if event_actor_id == actor_id && snapshot.outstanding_upload_count == 1
        ) {
            break;
        }
    }

    assert!(
        timeout(Duration::from_millis(80), remote.read_message())
            .await
            .is_err(),
        "the request must remain queued while the one-byte-per-second limiter has no tokens"
    );
    limiter.set_upload_rate(None);
    global_limiter.set_upload_rate(None);

    assert_eq!(
        timeout(Duration::from_millis(100), remote.read_message())
            .await
            .expect(
                "activated magnet actor should observe upload-rate changes without waiting for its old retry deadline"
            )
            .unwrap()
            .unwrap(),
        BtMessage::Piece {
            index: 0,
            begin: 0,
            data: vec![0x71; 8].into(),
        }
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}
