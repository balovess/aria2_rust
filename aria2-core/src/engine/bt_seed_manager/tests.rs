use super::*;
use crate::engine::bt_message_handler::PeerCommand;
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use aria2_protocol::bittorrent::peer::incoming::IncomingConnection;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[tokio::test]
async fn seeding_choke_decision_runs_when_interest_and_choke_state_mismatch() {
    let (mut manager, _client) = manager_with_dead_seed_peer(true, false).await;
    let actor_id = manager.swarm.iter().next().unwrap().actor_id;
    // Interested + choked needs an unchoke decision.
    {
        let stats = &mut manager.swarm.actor_mut(actor_id).unwrap().stats;
        stats.peer_interested = true;
        stats.am_choking = true;
    }
    assert!(manager.any_peer_choke_state_mismatch());

    // Not interested + unchoked needs a choke decision immediately.
    {
        let stats = &mut manager.swarm.actor_mut(actor_id).unwrap().stats;
        stats.peer_interested = false;
        stats.am_choking = false;
    }
    assert!(manager.any_peer_choke_state_mismatch());

    // Both settled states do not trigger an unnecessary choke round.
    manager.swarm.actor_mut(actor_id).unwrap().stats.am_choking = true;
    assert!(!manager.any_peer_choke_state_mismatch());
    {
        let stats = &mut manager.swarm.actor_mut(actor_id).unwrap().stats;
        stats.peer_interested = true;
        stats.am_choking = false;
    }
    assert!(!manager.any_peer_choke_state_mismatch());
}

#[tokio::test]
async fn seeding_manager_adopts_the_existing_torrent_peer_actor() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server, endpoint) = listener.accept().await.unwrap();
    let mut connection = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
        PeerConnection::from_stream_with_peer(server, [2u8; 20], false, false),
        endpoint,
    );
    connection.allocate_session_resource(16, 1, 16);
    connection.configure_upload_with_auto_unchoke(&BtSeedingConfig::default(), 1, 16, false);
    connection.stats.am_choking = true;
    let actor_id = connection.actor_id;
    let mut swarm = PeerSwarm::new(8);
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(16, 1));
    let provider_dyn: Arc<dyn crate::engine::bt_upload_session::PieceDataProvider> = provider;
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
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(16, 1));
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
    let existing = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
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
    manager.upload_sessions.push(
        crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
            PeerConnection::from_stream_with_peer(pending_stream, [3u8; 20], false, false),
            pending_endpoint,
        ),
    );
    assert_eq!(manager.num_sessions(), 2);

    manager.cancel();
    manager.run_seeding_loop().await.unwrap();

    assert!(manager.upload_sessions.is_empty());
    assert_eq!(manager.num_sessions(), 0);
    drop(existing_client.await.unwrap());
    drop(pending_client.await.unwrap());
}

async fn manager_with_dead_seed_peer(
    am_choking: bool,
    peer_interested: bool,
) -> (BtSeedManager, TcpStream) {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
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
    let mut connection = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
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
async fn peer_registry_reindexes_surviving_actor_after_dead_peer_removal() {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
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
    let first_connection = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
        PeerConnection::from_stream_with_peer(first_stream, [2u8; 20], false, false),
        first_endpoint,
    );
    let second_connection = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
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
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
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
    let existing = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
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
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(incoming)),
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
    snapshot.download_speed = 12.5;
    snapshot.avg_download_speed = 7;

    manager.apply_peer_event(
        crate::engine::bt_message_handler::PeerEvent::UploadQueueChanged {
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
    assert_eq!(peer_snapshot.download_speed, 12.5);
    assert_eq!(peer_snapshot.avg_download_speed, 7);
    manager.apply_peer_event(
        crate::engine::bt_message_handler::PeerEvent::PeerChokingChanged {
            actor_id,
            peer_choking: false,
        },
    );
    assert!(!manager.peer_snapshots()[0].peer_choking);
    manager.apply_peer_event(
        crate::engine::bt_message_handler::PeerEvent::PeerChokingChanged {
            actor_id,
            peer_choking: true,
        },
    );
    assert!(manager.peer_snapshots()[0].peer_choking);
}

#[tokio::test]
async fn seeding_accepts_a_peer_after_download_has_no_initial_peers() {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
    let (sender, receiver) = mpsc::channel(1);
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        1024,
        None,
        [1u8; 20],
        Some(receiver),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server_stream, endpoint) = listener.accept().await.unwrap();
    let _client_stream = client_task.await.unwrap();
    let peer_connection =
        PeerConnection::from_stream_with_peer(server_stream, [2u8; 20], false, false);
    sender
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(peer_connection)),
            endpoint,
        })
        .await
        .unwrap();

    manager.drain_incoming_peers().await;

    assert_eq!(manager.num_sessions(), 1);
}

#[tokio::test]
async fn cancelled_seeding_loop_future_preserves_incoming_peer_and_actor_channels() {
    let mut provider = crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1);
    provider.set_piece_data(0, vec![0x5a; 1024]);
    let provider = Arc::new(provider);
    let (sender, receiver) = mpsc::channel(1);
    let mut manager = BtSeedManager::new_with_transports(
        [7u8; 20],
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        1024,
        None,
        [1u8; 20],
        Some(receiver),
    );

    assert!(
        tokio::time::timeout(Duration::from_millis(10), manager.run_seeding_loop())
            .await
            .is_err()
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let cancel = manager.cancellation_token();
    let cancel_after_availability = cancel.clone();
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        let availability = read_bt_frame(&mut stream).await;
        assert!(matches!(availability.first(), Some(5)));
        assert_eq!(availability.get(1), Some(&0x80));
        cancel_after_availability.cancel();
    });
    let (server_stream, endpoint) = listener.accept().await.unwrap();
    let peer_connection =
        PeerConnection::from_stream_with_peer(server_stream, [2u8; 20], false, false);
    sender
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(peer_connection)),
            endpoint,
        })
        .await
        .unwrap();
    drop(sender);

    tokio::time::timeout(Duration::from_secs(2), manager.run_seeding_loop())
        .await
        .expect("seeding manager did not stop after actor cancellation")
        .expect("seeding manager returned an error");
    client.await.unwrap();
}

#[tokio::test]
async fn incoming_seed_peer_receives_piece_availability_before_interested() {
    let info_hash = [0x52u8; 20];
    let local_peer_id = [0x62u8; 20];
    let remote_peer_id = [0x72u8; 20];
    let piece = (0..16 * 1024)
        .map(|index| (index as u8).wrapping_mul(17))
        .collect::<Vec<_>>();
    let mut provider = crate::engine::bt_upload_session::InMemoryPieceProvider::new(16 * 1024, 1);
    provider.set_piece_data(0, piece.clone());
    let provider = Arc::new(provider);
    let piece_len = piece.len();
    let (sender, receiver) = mpsc::channel(1);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream
            .write_all(
                &aria2_protocol::bittorrent::message::handshake::Handshake::new(
                    &info_hash,
                    &remote_peer_id,
                )
                .to_bytes(),
            )
            .await
            .unwrap();

        let mut response = [0u8; 68];
        tokio::io::AsyncReadExt::read_exact(&mut stream, &mut response)
            .await
            .unwrap();
        assert_eq!(
            aria2_protocol::bittorrent::message::handshake::Handshake::parse(&response)
                .unwrap()
                .info_hash,
            info_hash
        );

        let availability = read_bt_frame(&mut stream).await;
        assert!(matches!(availability.first(), Some(5)));
        assert_eq!(availability.get(1), Some(&0x80));

        stream.write_all(&[0, 0, 0, 1, 2]).await.unwrap();
        loop {
            let payload = read_bt_frame(&mut stream).await;
            assert!(!payload.is_empty(), "seed peer closed before unchoking");
            if payload[0] == 1 {
                break;
            }
        }

        let mut request = Vec::with_capacity(17);
        request.extend_from_slice(&13u32.to_be_bytes());
        request.push(6);
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&0u32.to_be_bytes());
        request.extend_from_slice(&(piece_len as u32).to_be_bytes());
        stream.write_all(&request).await.unwrap();

        let payload = read_bt_frame(&mut stream).await;
        assert_eq!(payload.first().copied(), Some(7));
        assert_eq!(u32::from_be_bytes(payload[1..5].try_into().unwrap()), 0);
        assert_eq!(u32::from_be_bytes(payload[5..9].try_into().unwrap()), 0);
        assert_eq!(&payload[9..], piece.as_slice());
    });

    let (server_stream, endpoint) = listener.accept().await.unwrap();
    let connection =
        PeerConnection::from_incoming_stream(server_stream, &info_hash, &local_peer_id)
            .await
            .unwrap();
    sender
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(connection)),
            endpoint,
        })
        .await
        .unwrap();
    drop(sender);

    let mut manager = BtSeedManager::new_with_transports(
        info_hash,
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::with_ratio(1.0),
        16 * 1024,
        None,
        local_peer_id,
        Some(receiver),
    );
    manager.drain_incoming_peers().await;
    assert_eq!(
        manager.swarm.len(),
        1,
        "incoming connection must join swarm registry"
    );
    assert!(
        manager.upload_sessions.is_empty(),
        "actorized peer must not remain a raw session"
    );

    tokio::time::timeout(Duration::from_secs(5), manager.run_seeding_loop())
        .await
        .expect("seed manager did not finish after incoming upload")
        .expect("seed manager returned an error");
    client.await.unwrap();

    assert_eq!(manager.total_uploaded(), piece_len as u64);
    assert!(manager.halt_requested(), "ratio exit should request halt");
}

#[tokio::test]
async fn incoming_seed_peer_is_not_kept_as_a_raw_connection_when_swarm_is_closed() {
    let info_hash = [0x42u8; 20];
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
    let (sender, receiver) = mpsc::channel(1);
    let mut manager = BtSeedManager::new_with_transports(
        info_hash,
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        0,
        None,
        [0x52u8; 20],
        Some(receiver),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server, endpoint) = listener.accept().await.unwrap();
    let connection = PeerConnection::from_stream_with_peer(server, [0x62u8; 20], false, false);
    sender
        .send(crate::engine::bt_peer_listener::IncomingPeer {
            connection: IncomingConnection::Plain(Box::new(connection)),
            endpoint,
        })
        .await
        .unwrap();
    drop(sender);

    manager.swarm.close_event_receiver();
    manager.drain_incoming_peers().await;

    assert!(manager.swarm.is_empty());
    assert!(manager.upload_sessions.is_empty());
    drop(client.await.unwrap());
}

#[tokio::test]
async fn initial_seed_peer_is_not_kept_raw_when_actor_startup_is_closed() {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
    let mut manager = BtSeedManager::new(
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        0,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::spawn(async move { TcpStream::connect(address).await.unwrap() });
    let (server, endpoint) = listener.accept().await.unwrap();
    let connection = crate::engine::bt_peer_connection::BtPeerConn::from_incoming_plain(
        PeerConnection::from_stream_with_peer(server, [0x72u8; 20], false, false),
        endpoint,
    );
    manager.upload_sessions.push(connection);
    manager.swarm.close_event_receiver();
    manager.swarm.close_event_sender();
    manager.cancellation_token().cancel();

    manager.run_seeding_loop().await.unwrap();

    assert!(manager.upload_sessions.is_empty());
    assert!(manager.swarm.is_empty());
    drop(client.await.unwrap());
}

async fn read_bt_frame(stream: &mut TcpStream) -> Vec<u8> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length).await.unwrap();
    let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
    stream.read_exact(&mut payload).await.unwrap();
    payload
}

#[tokio::test]
async fn seeding_does_not_end_just_because_all_peers_disconnect() {
    let provider = Arc::new(crate::engine::bt_upload_session::InMemoryPieceProvider::new(1024, 1));
    let mut manager = BtSeedManager::new(
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::with_time(1),
        1024,
    );
    let cancel = manager.cancellation_token();
    let task = tokio::spawn(async move {
        let result = manager.run_seeding_loop().await;
        (result, manager.seeding_duration(), manager.halt_requested())
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let (_, duration, halt_requested) = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();

    assert!(duration >= Duration::from_millis(40));
    assert!(!halt_requested);
}
