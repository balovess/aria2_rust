use super::*;

#[tokio::test(start_paused = true)]
async fn peer_actor_keeps_connection_on_keepalive_without_piece_progress() {
    use tokio::time::{Duration, advance, timeout};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let remote_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (local_stream, endpoint) = listener.accept().await.unwrap();
    let local = PeerConnection::from_stream_with_peer(local_stream, [0; 20], false, true);
    let mut connection = BtPeerConn::from_incoming_tcp(local, endpoint);
    connection.set_timeouts(Duration::from_secs(120), Duration::from_secs(80));
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

    remote.send_message(&BtMessage::Interested).await.unwrap();
    loop {
        if matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .expect("peer actor event channel closed"),
            PeerEvent::InterestChanged { actor_id: event_actor_id, .. }
                if event_actor_id == actor_id
        ) {
            break;
        }
    }

    advance(Duration::from_secs(50)).await;
    remote.send_message(&BtMessage::KeepAlive).await.unwrap();
    remote.send_message(&BtMessage::Unchoke).await.unwrap();
    loop {
        if matches!(
            timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .expect("peer actor event channel closed"),
            PeerEvent::PeerChokingChanged {
                actor_id: event_actor_id,
                peer_choking: false,
            } if event_actor_id == actor_id
        ) {
            break;
        }
    }

    advance(Duration::from_secs(15)).await;
    tokio::task::yield_now().await;
    assert!(
        !worker.is_finished(),
        "valid keepalive/control traffic must keep the connection alive until bt-timeout"
    );

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn peer_actor_downloads_a_block_over_utp_after_handshake_handoff() {
    use aria2_protocol::bittorrent::message::handshake::Handshake;
    use aria2_protocol::bittorrent::message::serializer::serialize;
    use aria2_protocol::bittorrent::utp::UtpSocket;

    let info_hash = [0x41; 20];
    let local_peer_id = [0x52; 20];
    let remote_peer_id = [0x63; 20];
    let mut server = UtpSocket::bind("127.0.0.1:0").expect("bind uTP test peer");
    let address = server.local_addr();
    let server_task = tokio::spawn(async move {
        let mut request_buffer = Vec::new();
        loop {
            for (connection_id, bytes) in server
                .poll_recv()
                .expect("uTP test peer should process incoming packets")
            {
                request_buffer.extend_from_slice(&bytes);
                if request_buffer.len() >= 68 {
                    let request = Handshake::parse(&request_buffer[..68])
                        .expect("client should send a valid BT handshake");
                    assert_eq!(request.info_hash, info_hash);
                    assert_eq!(request.peer_id, local_peer_id);
                    server
                        .send(
                            connection_id,
                            &Handshake::new(&info_hash, &remote_peer_id).to_bytes(),
                        )
                        .expect("uTP test peer should answer BT handshake");
                    return (server, connection_id);
                }
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let client_socket = Arc::new(tokio::sync::Mutex::new(
        UtpSocket::bind("127.0.0.1:0").expect("bind uTP client socket"),
    ));
    let mut connection = BtPeerConn::connect_utp_with_policy(
        address,
        &info_hash,
        None,
        crate::engine::bittorrent::peer::connection::UtpConnectionOptions {
            local_peer_id,
            timeout: Duration::from_secs(2),
            listen_port: None,
            shared_socket: Some(client_socket),
            dht_enabled: false,
        },
        &crate::network::OutboundNetworkPolicy::direct(),
    )
    .await
    .expect("BT handshake should complete over uTP");
    assert_eq!(
        connection.connection_type,
        crate::engine::bittorrent::peer::connection::ConnectionType::Utp
    );
    let (mut server, connection_id) = tokio::time::timeout(Duration::from_secs(2), server_task)
        .await
        .expect("uTP peer should finish the BitTorrent handshake")
        .expect("uTP peer task should not panic");

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

    server
        .send(connection_id, &serialize(&BtMessage::Unchoke))
        .expect("uTP peer should unchoke the actor");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("actor should process the uTP Unchoke")
            .expect("peer actor event channel should remain open");
        if matches!(
            event,
            PeerEvent::PeerChokingChanged {
                actor_id: event_actor_id,
                peer_choking: false,
            } if event_actor_id == actor_id
        ) {
            break;
        }
    }

    let generation = RequestGeneration::allocate();
    let request = BlockRequest {
        block_index: 0,
        offset: 0,
        length: 16,
    };
    command_tx.begin_generation(generation, 0).unwrap();
    command_tx
        .send(PeerCommand::Request {
            generation,
            piece_index: 0,
            request,
        })
        .await
        .unwrap();
    let expected_request = serialize(&BtMessage::Request {
        request: PieceBlockRequest::new(0, 0, 16),
    });
    let request_bytes = tokio::time::timeout(Duration::from_secs(2), async {
        let mut received = Vec::new();
        loop {
            for (received_connection_id, bytes) in server
                .poll_recv()
                .expect("uTP peer should receive actor packets")
            {
                assert_eq!(received_connection_id, connection_id);
                received.extend_from_slice(&bytes);
                if received
                    .windows(expected_request.len())
                    .any(|window| window == expected_request)
                {
                    return received;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("uTP peer should receive the actor's block request");
    assert!(
        request_bytes
            .windows(expected_request.len())
            .any(|window| window == expected_request)
    );

    let block = vec![0xA5; 16];
    server
        .send(
            connection_id,
            &serialize(&BtMessage::Piece {
                index: 0,
                begin: 0,
                data: block.clone().into(),
            }),
        )
        .expect("uTP peer should return the requested block");
    loop {
        let event = tokio::time::timeout(Duration::from_secs(2), event_rx.recv())
            .await
            .expect("actor should receive the requested block over uTP")
            .expect("peer actor event channel should remain open");
        if let PeerEvent::Message {
            actor_id: event_actor_id,
            message: BtMessage::Piece { index, begin, data },
            ..
        } = event
        {
            assert_eq!(event_actor_id, actor_id);
            assert_eq!(index, 0);
            assert_eq!(begin, 0);
            assert_eq!(data.as_ref(), block);
            break;
        }
    }

    command_tx.send(PeerCommand::Shutdown).await.unwrap();
    worker.await.unwrap();
}
