use super::*;

#[tokio::test]
async fn active_download_actor_serves_upload_request_on_same_peer_connection() {
    let info_hash = [0x41u8; 20];
    let local_peer_id = [0x42u8; 20];
    let remote_peer_id = [0x43u8; 20];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_piece = vec![0xB7u8; 16];
    let locally_verified_piece = vec![0xA5u8; 16];

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
            .write_all(&serialize(&BtMessage::Bitfield { data: vec![0x40] }))
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Unchoke))
            .await
            .unwrap();

        let request = read_frame(&mut stream).await;
        assert_eq!(request.first().copied(), Some(6));
        assert_eq!(u32::from_be_bytes(request[1..5].try_into().unwrap()), 1);
        assert_eq!(u32::from_be_bytes(request[5..9].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(request[9..13].try_into().unwrap()), 16);
        stream
            .write_all(&serialize(&BtMessage::Interested))
            .await
            .unwrap();
        assert_eq!(read_frame(&mut stream).await.as_slice(), &[1]);
        stream
            .write_all(&serialize(&BtMessage::Request {
                request: aria2_protocol::bittorrent::message::types::PieceBlockRequest {
                    index: 0,
                    begin: 0,
                    length: 16,
                },
            }))
            .await
            .unwrap();
        let mut pex = aria2_protocol::bittorrent::message::extension::UtPexMessage::new();
        pex.added.push(
            aria2_protocol::bittorrent::message::extension::CompactPeerV4([
                127, 0, 0, 1, 0x1a, 0xe2,
            ]),
        );
        stream
            .write_all(&serialize(&BtMessage::Extended {
                ext_id: 9,
                payload: pex.to_payload(),
            }))
            .await
            .unwrap();
        stream
            .write_all(&serialize(&BtMessage::Piece {
                index: 1,
                begin: 0,
                data: remote_piece.into(),
            }))
            .await
            .unwrap();

        for _ in 0..3 {
            let frame = read_frame(&mut stream).await;
            if frame.first().copied() == Some(7) {
                assert_eq!(u32::from_be_bytes(frame[1..5].try_into().unwrap()), 0);
                assert_eq!(u32::from_be_bytes(frame[5..9].try_into().unwrap()), 0);
                return frame[9..].to_vec();
            }
        }
        panic!("local peer did not upload its verified piece")
    });

    let peer_addr = PeerAddr::new("127.0.0.1", address.port());
    let mut connection = BtPeerConn::connect_plain_with_policy(
        &peer_addr,
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
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        2,
        16,
        false,
    );
    connection.set_pex_enabled(true);
    connection.register_peer_extension("ut_pex", 9);

    let mut choking_algo = ChokingAlgorithm::new(ChokingConfig::default());
    choking_algo.add_peer(connection.stats().clone());
    let mut provider = InMemoryPieceProvider::new(16, 2);
    provider.set_piece_data(0, locally_verified_piece.clone());
    let provider: Arc<dyn PieceDataProvider> = Arc::new(provider);
    let actor_id = connection.actor_id;
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(
        swarm
            .spawn_peer(connection, None, Arc::clone(&provider))
            .is_ok()
    );
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        download_piece_blocks(
            &mut swarm,
            1,
            16,
            1,
            Duration::from_secs(2),
            1,
            Some(&mut choking_algo),
        ),
    )
    .await
    .expect("active download worker timed out")
    .unwrap();

    assert_eq!(result.piece.unwrap().data, vec![0xB7; 16]);
    assert_eq!(result.pex_peers.len(), 1);
    assert_eq!(result.pex_peers[0].ip, "127.0.0.1");
    assert_eq!(result.pex_peers[0].port, 6882);
    assert_eq!(remote.await.unwrap(), locally_verified_piece);
    tokio::time::timeout(Duration::from_secs(2), async {
        while swarm.actor(actor_id).unwrap().stats.uploaded_bytes != 16 {
            let mut events = swarm
                .lease_event_receiver()
                .expect("peer event receiver must be available after download completion");
            events
                .recv()
                .await
                .expect("peer actor must publish its completed upload");
        }
    })
    .await
    .expect("upload byte event must update the peer actor snapshot");
    let stats = &swarm.actor(actor_id).unwrap().stats;
    assert_eq!(stats.uploaded_bytes, 16);
    assert!(stats.upload_speed > 0.0);
    assert!(!stats.am_choking);
    assert!(choking_algo.peers()[0].peer_interested);
    assert!(!choking_algo.peers()[0].am_choking);
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn swarm_actor_downloads_consecutive_pieces_without_restarting_peer_io() {
    let info_hash = [0x71u8; 20];
    let local_peer_id = [0x72u8; 20];
    let remote_peer_id = [0x73u8; 20];
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
            if expected_piece == 0 {
                let mut pex = aria2_protocol::bittorrent::message::extension::UtPexMessage::new();
                pex.added.push(
                    aria2_protocol::bittorrent::message::extension::CompactPeerV4([
                        127, 0, 0, 1, 0x1a, 0xe1,
                    ]),
                );
                stream
                    .write_all(&serialize(&BtMessage::Extended {
                        ext_id: 9,
                        payload: pex.to_payload(),
                    }))
                    .await
                    .unwrap();
            }
            stream
                .write_all(&serialize(&BtMessage::Piece {
                    index: expected_piece,
                    begin: 0,
                    data: vec![0x80 + expected_piece as u8; 16].into(),
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
    connection.set_pex_enabled(true);
    connection.register_peer_extension("ut_pex", 9);
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 2));
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(matches!(
        swarm.spawn_peer(connection, None, provider),
        Ok(registered_actor_id) if registered_actor_id == actor_id
    ));

    for piece_index in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            download_piece_blocks(
                &mut swarm,
                piece_index,
                16,
                1,
                Duration::from_secs(2),
                1,
                None,
            ),
        )
        .await
        .expect("swarm piece download timed out")
        .unwrap();

        assert_eq!(
            result.piece.unwrap().data,
            vec![0x80 + piece_index as u8; 16]
        );
        assert_eq!(result.peer_actor_ids, vec![actor_id]);
        if piece_index == 0 {
            assert_eq!(result.pex_peers.len(), 1);
            assert_eq!(result.pex_peers[0].ip, "127.0.0.1");
            assert_eq!(result.pex_peers[0].port, 6881);
        }
        assert_eq!(swarm.len(), 1);
        assert!(swarm.actor(actor_id).is_some());
    }

    swarm.shutdown_all().await;
    remote.await.unwrap();
}
