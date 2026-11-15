use super::*;

#[tokio::test]
async fn normal_piece_retry_does_not_add_a_fixed_batch_delay() {
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(1);

    let result = tokio::time::timeout(
        Duration::from_millis(80),
        download_piece_blocks(&mut swarm, 0, 16, 1, Duration::from_millis(1), 2, None),
    )
    .await
    .expect("normal piece retries should not wait an unrelated fixed interval");

    assert!(
        result
            .expect("empty swarm should return a piece outcome")
            .piece
            .is_err(),
        "an empty swarm cannot complete the piece"
    );
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn endgame_piece_retry_does_not_add_a_fixed_batch_delay() {
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(1);
    let mut endgame = EndgameState::new();
    endgame.enter_endgame();

    let result = tokio::time::timeout(
        Duration::from_millis(80),
        download_piece_blocks_endgame(
            &mut swarm,
            0,
            16,
            1,
            &mut endgame,
            Duration::from_millis(1),
            2,
            None,
            &[],
            None,
        ),
    )
    .await
    .expect("endgame piece retries should not wait an unrelated fixed interval");

    assert!(
        result
            .expect("empty endgame swarm should return a piece outcome")
            .piece
            .is_err(),
        "an empty swarm cannot complete the piece"
    );
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn swarm_actor_endgame_uses_the_same_peer_across_piece_generations() {
    let info_hash = [0x75u8; 20];
    let local_peer_id = [0x76u8; 20];
    let remote_peer_id = [0x77u8; 20];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    let remote = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request_handshake = [0u8; 68];
        stream.read_exact(&mut request_handshake).await.unwrap();
        assert_eq!(&request_handshake[28..48], info_hash.as_slice());
        stream
            .write_all(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Bitfield { data: vec![0xC0] }))
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Unchoke))
            .await
            .unwrap();

        for expected_piece in 0..2 {
            let request = read_frame(&mut stream).await;
            assert_eq!(request.first().copied(), Some(6));
            assert_eq!(
                u32::from_be_bytes(request[1..5].try_into().unwrap()),
                expected_piece
            );
            assert_eq!(u32::from_be_bytes(request[5..9].try_into().unwrap()), 0);
            assert_eq!(u32::from_be_bytes(request[9..13].try_into().unwrap()), 16);
            stream
                .write_all(&serialize(&BtMessage::Piece {
                    index: expected_piece,
                    begin: 0,
                    data: vec![0x90 + expected_piece as u8; 16].into(),
                }))
                .await
                .unwrap();
        }

        let mut closed = [0u8; 1];
        let _ = stream.read(&mut closed).await;
    });

    let mut connection = BtPeerConn::connect_plain_with_policy(
        &PeerAddr::new("127.0.0.1", address.port()),
        &info_hash,
        None,
        &local_peer_id,
        Duration::from_secs(5),
        false,
        &crate::network::OutboundNetworkPolicy::direct(),
    )
    .await
    .unwrap();
    connection.allocate_session_resource(16, 2, 32);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 2));
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(matches!(
        swarm.spawn_peer(connection, None, provider),
        Ok(registered_actor_id) if registered_actor_id == actor_id
    ));

    let mut endgame_state = EndgameState::new();
    for piece_index in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            download_piece_blocks_endgame(
                &mut swarm,
                piece_index,
                16,
                1,
                &mut endgame_state,
                Duration::from_secs(2),
                1,
                None,
                &[],
                None,
            ),
        )
        .await
        .expect("swarm endgame piece download timed out")
        .unwrap();

        assert_eq!(
            result.piece.unwrap().data,
            vec![0x90 + piece_index as u8; 16]
        );
        assert_eq!(result.peer_actor_ids, vec![actor_id]);
        assert_eq!(swarm.len(), 1);
        assert!(swarm.actor(actor_id).is_some());
    }

    swarm.shutdown_all().await;
    remote.await.unwrap();
}

#[tokio::test]
async fn failed_endgame_piece_preserves_tracker_and_pex_discovery() {
    let info_hash = [0x79u8; 20];
    let local_peer_id = [0x7Au8; 20];
    let remote_peer_id = [0x7Bu8; 20];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();

    let remote = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request_handshake = [0u8; 68];
        stream.read_exact(&mut request_handshake).await.unwrap();
        stream
            .write_all(&Handshake::new(&info_hash, &remote_peer_id).to_bytes())
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Bitfield { data: vec![0x80] }))
            .await
            .unwrap();
        let mut closed = [0u8; 1];
        let _ = stream.read(&mut closed).await;
    });

    let mut connection = BtPeerConn::connect_plain_with_policy(
        &PeerAddr::new("127.0.0.1", address.port()),
        &info_hash,
        None,
        &local_peer_id,
        Duration::from_secs(5),
        false,
        &crate::network::OutboundNetworkPolicy::direct(),
    )
    .await
    .unwrap();
    connection.allocate_session_resource(16, 1, 32);
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    let tracker_peer = PeerAddr::new("127.0.0.1", 6883);
    let pex_peer = PeerAddr::new("127.0.0.1", 6884);
    let event_tx = swarm.event_sender().unwrap();
    event_tx
        .send(
            crate::engine::bittorrent::peer::message_handler::PeerEvent::TrackerPeers {
                peers: vec![tracker_peer.clone()],
            },
        )
        .await
        .unwrap();
    event_tx
        .send(
            crate::engine::bittorrent::peer::message_handler::PeerEvent::PexPeers {
                peers: vec![pex_peer.clone()],
            },
        )
        .await
        .unwrap();

    let mut endgame_state = EndgameState::new();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        download_piece_blocks_endgame(
            &mut swarm,
            0,
            16,
            1,
            &mut endgame_state,
            Duration::from_millis(50),
            1,
            None,
            &[],
            None,
        ),
    )
    .await
    .expect("failed endgame piece attempt timed out")
    .expect("piece attempt outcome should retain its discovery events");

    assert!(
        result.piece.is_err(),
        "the peer remained choking this client"
    );
    assert_eq!(result.tracker_peers, vec![tracker_peer]);
    assert_eq!(result.pex_peers, vec![pex_peer]);

    swarm.shutdown_all().await;
    remote.await.unwrap();
}
