use std::sync::Arc;
use std::time::Duration;

use super::super::{download_piece_blocks, download_piece_blocks_endgame};
use crate::engine::bittorrent::download::execute::EndgameState;
use crate::engine::bittorrent::peer::choking_algorithm::{ChokingAlgorithm, ChokingConfig};
use crate::engine::bittorrent::peer::connection::BtPeerConn;
use crate::engine::bittorrent::peer::upload_session::{
    BtSeedingConfig, InMemoryPieceProvider, PieceDataProvider,
};
use aria2_protocol::bittorrent::message::handshake::Handshake;
use aria2_protocol::bittorrent::message::serializer::serialize;
use aria2_protocol::bittorrent::message::types::BtMessage;
use aria2_protocol::bittorrent::peer::connection::PeerAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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
    let mut event_lease = swarm.lease_event_receiver().unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if matches!(event_lease.recv().await, Some(crate::engine::bittorrent::peer::message_handler::PeerEvent::UploadBytes { actor_id: event_actor, .. }) if event_actor == actor_id) {
                break;
            }
        }
    })
    .await
    .expect("uploaded-byte actor event was not published");
    drop(event_lease);
    let stats = &swarm.actor(actor_id).unwrap().stats;
    assert_eq!(stats.uploaded_bytes, 16);
    assert!(stats.upload_speed > 0.0);
    assert!(!stats.am_choking);
    assert!(choking_algo.peers()[0].peer_interested);
    assert!(!choking_algo.peers()[0].am_choking);
    assert_eq!(remote.await.unwrap(), locally_verified_piece);
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

#[tokio::test]
async fn active_download_updates_choke_peer_stats_before_piece_completion() {
    const BLOCK_LEN: usize = 16 * 1024;
    let info_hash = [0x51u8; 20];
    let local_peer_id = [0x52u8; 20];
    let remote_peer_id = [0x53u8; 20];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release_remote, hold_remote) = tokio::sync::oneshot::channel();
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
        stream
            .write_all(&serialize(&BtMessage::Unchoke))
            .await
            .unwrap();

        let mut requested_offsets = Vec::new();
        while requested_offsets.len() < 2 {
            let frame = read_frame(&mut stream).await;
            if frame.first().copied() == Some(6) {
                requested_offsets.push(u32::from_be_bytes(frame[5..9].try_into().unwrap()));
            }
        }
        assert!(requested_offsets.contains(&0));
        stream
            .write_all(&serialize(&BtMessage::Piece {
                index: 0,
                begin: 0,
                data: vec![0x71; BLOCK_LEN].into(),
            }))
            .await
            .unwrap();
        let mut closed = [0u8; 1];
        let _ = stream.read(&mut closed).await;
        let _ = hold_remote.await;
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
    connection.allocate_session_resource((2 * BLOCK_LEN) as u32, 1, (2 * BLOCK_LEN) as u64);
    let mut choking_algo = ChokingAlgorithm::new(ChokingConfig::default());
    choking_algo.add_peer(connection.stats().clone());
    let actor_id = connection.actor_id;
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());

    let result = tokio::time::timeout(
        Duration::from_secs(3),
        download_piece_blocks(
            &mut swarm,
            0,
            (2 * BLOCK_LEN) as u32,
            2,
            Duration::from_millis(200),
            1,
            Some(&mut choking_algo),
        ),
    )
    .await
    .expect("incomplete piece attempt did not time out");

    assert!(
        result
            .expect("incomplete piece should return a piece outcome")
            .piece
            .is_err()
    );
    assert_eq!(choking_algo.peers()[0].downloaded_bytes, BLOCK_LEN as u64);
    assert_eq!(
        swarm.actor(actor_id).unwrap().stats.downloaded_bytes,
        BLOCK_LEN as u64
    );
    release_remote.send(()).unwrap();
    remote.await.unwrap();
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn active_download_applies_choke_rotation_deadline_without_peer_messages() {
    let info_hash = [0x35u8; 20];
    let local_peer_id = [0x36u8; 20];
    let remote_peer_id = [0x37u8; 20];
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
        stream
            .write_all(&serialize(&BtMessage::Unchoke))
            .await
            .unwrap();

        wait_for_request_and_unchoke(&mut stream).await;
        vec![1]
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
    connection.allocate_session_resource(16, 1, 16);
    connection.stats.peer_interested = true;
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        16,
        false,
    );

    let mut choking_algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 2,
        choke_rotation_interval_secs: 1,
        ..ChokingConfig::default()
    });
    choking_algo.add_peer(connection.stats().clone());
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let actor_id = connection.actor_id;
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        download_piece_blocks(
            &mut swarm,
            0,
            16,
            1,
            Duration::from_secs(3),
            1,
            Some(&mut choking_algo),
        ),
    )
    .await
    .expect("piece attempt did not respect request timeout");

    assert!(
        result
            .expect("incomplete piece should return a piece outcome")
            .piece
            .is_err()
    );
    assert!(!swarm.actor(actor_id).unwrap().stats.am_choking);
    assert_eq!(remote.await.unwrap().as_slice(), &[1]);
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn endgame_applies_choke_rotation_deadline_without_peer_messages() {
    let info_hash = [0x45u8; 20];
    let local_peer_id = [0x46u8; 20];
    let remote_peer_id = [0x47u8; 20];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release_remote, hold_remote) = tokio::sync::oneshot::channel();
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
        stream
            .write_all(&serialize(&BtMessage::Unchoke))
            .await
            .unwrap();

        wait_for_request_and_unchoke(&mut stream).await;
        let _ = hold_remote.await;
        vec![1]
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
    connection.allocate_session_resource(16, 1, 16);
    connection.stats.peer_interested = true;
    connection.configure_upload_with_auto_unchoke(
        &BtSeedingConfig::default(),
        crate::rate_limiter::RateLimiter::unlimited(),
        1,
        16,
        false,
    );

    let mut choking_algo = ChokingAlgorithm::new(ChokingConfig {
        max_upload_slots: 2,
        choke_rotation_interval_secs: 1,
        ..ChokingConfig::default()
    });
    choking_algo.add_peer(connection.stats().clone());
    let mut endgame_state = EndgameState::new();
    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let actor_id = connection.actor_id;
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    assert!(swarm.spawn_peer(connection, None, provider).is_ok());
    let result = tokio::time::timeout(
        Duration::from_secs(6),
        download_piece_blocks_endgame(
            &mut swarm,
            0,
            16,
            1,
            &mut endgame_state,
            Duration::from_secs(3),
            1,
            Some(&mut choking_algo),
        ),
    )
    .await
    .expect("endgame piece attempt did not respect request timeout");

    assert!(
        result
            .expect("incomplete endgame piece should return a piece outcome")
            .piece
            .is_err()
    );
    assert!(!choking_algo.peers()[0].am_choking);
    assert!(!swarm.actor(actor_id).unwrap().stats.am_choking);
    release_remote.send(()).unwrap();
    assert_eq!(remote.await.unwrap().as_slice(), &[1]);
    swarm.shutdown_all().await;
}

#[tokio::test]
async fn swarm_endgame_actors_duplicate_requests_and_cancel_loser() {
    let info_hash = [0x51u8; 20];
    let local_peer_id = [0x52u8; 20];
    let remote_peer_ids = [[0x53u8; 20], [0x54u8; 20]];
    let piece_data = vec![0xC7u8; 16];
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut addresses = Vec::new();
    let mut remote_tasks = Vec::new();

    for (peer_index, remote_peer_id) in remote_peer_ids.into_iter().enumerate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        addresses.push(listener.local_addr().unwrap());
        let barrier = Arc::clone(&barrier);
        let piece_data = piece_data.clone();
        remote_tasks.push(tokio::spawn(async move {
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
            stream
                .write_all(&serialize(&BtMessage::Unchoke))
                .await
                .unwrap();

            let request = read_frame(&mut stream).await;
            assert_eq!(request.first().copied(), Some(6));
            assert_eq!(u32::from_be_bytes(request[1..5].try_into().unwrap()), 0);
            assert_eq!(u32::from_be_bytes(request[5..9].try_into().unwrap()), 0);
            assert_eq!(u32::from_be_bytes(request[9..13].try_into().unwrap()), 16);
            barrier.wait().await;

            if peer_index == 0 {
                stream
                    .write_all(&serialize(&BtMessage::Have { piece_index: 1 }))
                    .await
                    .unwrap();
                stream
                    .write_all(&serialize(&BtMessage::Piece {
                        index: 0,
                        begin: 0,
                        data: piece_data.into(),
                    }))
                    .await
                    .unwrap();
            } else {
                let cancel = read_frame(&mut stream).await;
                assert_eq!(cancel.first().copied(), Some(8));
                assert_eq!(u32::from_be_bytes(cancel[1..5].try_into().unwrap()), 0);
                assert_eq!(u32::from_be_bytes(cancel[5..9].try_into().unwrap()), 0);
                assert_eq!(u32::from_be_bytes(cancel[9..13].try_into().unwrap()), 16);
            }
        }));
    }

    let provider: Arc<dyn PieceDataProvider> = Arc::new(InMemoryPieceProvider::new(16, 1));
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(16);
    let mut actor_ids = Vec::new();
    for address in addresses {
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
        let actor_id = connection.actor_id;
        assert!(matches!(
            swarm.spawn_peer(connection, None, Arc::clone(&provider)),
            Ok(registered_actor_id) if registered_actor_id == actor_id
        ));
        actor_ids.push(actor_id);
    }

    let mut endgame_state = EndgameState::new();
    endgame_state.enter_endgame();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        download_piece_blocks_endgame(
            &mut swarm,
            0,
            16,
            1,
            &mut endgame_state,
            Duration::from_secs(2),
            1,
            None,
        ),
    )
    .await
    .expect("endgame worker timed out")
    .unwrap();

    let piece = result.piece.unwrap();
    assert_eq!(piece.data, vec![0xC7; 16]);
    assert_eq!(piece.peer_bytes.len(), 1);
    assert_eq!(result.availability_changed_actor_ids.len(), 2);
    assert!(actor_ids.iter().any(|actor_id| {
        result.availability_changed_actor_ids.contains(actor_id)
            && swarm.actor(*actor_id).is_some_and(|actor| {
                actor
                    .bitfield
                    .first()
                    .is_some_and(|bitfield| bitfield & 0x40 != 0)
            })
    }));
    assert_eq!(
        actor_ids[piece.peer_bytes[0].peer_index],
        result.peer_actor_ids[0]
    );
    assert_eq!(
        swarm.len(),
        2,
        "endgame completion must not stop either peer actor"
    );
    assert!(endgame_state.get_cancel_targets(0, 0, 16).is_empty());
    for task in remote_tasks {
        task.await.unwrap();
    }
    swarm.shutdown_all().await;
}

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut length))
        .await
        .expect("peer frame timed out")
        .unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}

async fn wait_for_request_and_unchoke(stream: &mut TcpStream) {
    let mut unchoked = false;
    loop {
        let frame = read_frame(stream).await;
        match frame.first().copied() {
            Some(1) => unchoked = true,
            Some(6) => break,
            _ => {}
        }
    }
    if !unchoked {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if read_frame(stream).await.first().copied() == Some(1) {
                    return;
                }
            }
        })
        .await
        .expect("choke scheduler did not unchoke the interested peer");
    }
}
