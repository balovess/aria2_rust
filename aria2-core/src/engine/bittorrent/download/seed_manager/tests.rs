use super::*;
use crate::engine::bittorrent::peer::message_handler::{PeerCommand, PeerEvent};
use aria2_protocol::bittorrent::peer::connection::PeerConnection;
use std::net::{Ipv4Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

#[tokio::test]
async fn seeding_discovery_sources_survive_storage_and_actor_admission() {
    let info_hash = [0x71; 20];
    let mut endpoints = Vec::new();
    let mut peers = Vec::new();
    for remote_peer_id in [[0x72; 20], [0x74; 20], [0x76; 20]] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        endpoints.push(listener.local_addr().unwrap());
        peers.push(tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                if let Ok(incoming) =
                    aria2_protocol::bittorrent::peer::incoming::receive(stream, &[info_hash]).await
                    && let Ok(connection) = incoming.complete(remote_peer_id, None, false).await
                {
                    return connection;
                }
            }
        }));
    }

    let options = crate::request::request_group::DownloadOptions {
        enable_utp: false,
        enable_peer_exchange: true,
        bt_max_peers: 3,
        ..crate::request::request_group::DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(704),
            Vec::new(),
            options.clone(),
        ),
    ));
    let peer_storage = Arc::new(std::sync::Mutex::new(
        crate::engine::bittorrent::peer::storage::DefaultPeerStorage::new(),
    ));
    let connection_options =
        crate::engine::bittorrent::peer::interaction::BtPeerConnectionOptions::from_download_options(
            &options, [0x73; 20],
        );
    let discovery = super::SeedPeerDiscovery {
        group,
        dht_engines: crate::engine::bittorrent::dht::engine_set::DhtEngineSet::default(),
        dht_lookup: crate::engine::bittorrent::download::execute::DhtPeriodicLookup::new(),
        listen_port: 0,
        connection_options,
        total_size: 16,
        utp_socket: None,
        outbound_network_policy: Arc::new(crate::network::OutboundNetworkPolicy::direct()),
        enable_peer_exchange: true,
    };
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1),
    );
    let provider_dyn: Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider> =
        provider;
    let mut manager = BtSeedManager::new_with_transports(
        info_hash,
        Vec::new(),
        provider_dyn,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        None,
        [0x73; 20],
        None,
    )
    .with_peer_storage(Arc::clone(&peer_storage))
    .with_peer_discovery(discovery);

    let tracker_peer = (endpoints[0].ip().to_string(), endpoints[0].port());
    manager.store_tracker_peers(vec![tracker_peer.clone(), tracker_peer]);
    manager.apply_peer_event(
        crate::engine::bittorrent::peer::message_handler::PeerEvent::PexPeers {
            peers: vec![
                aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                    &endpoints[0].ip().to_string(),
                    endpoints[0].port(),
                ),
                aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
                    &endpoints[1].ip().to_string(),
                    endpoints[1].port(),
                ),
            ],
        },
    );
    manager.store_peer_addresses(
        vec![aria2_protocol::bittorrent::peer::connection::PeerAddr::new(
            &endpoints[2].ip().to_string(),
            endpoints[2].port(),
        )],
        crate::request::request_group::BtPeerSource::Dht,
    );
    assert_eq!(peer_storage.lock().unwrap().count_all_peers(), 3);

    manager.start_peer_connection_attempt();
    let result = {
        let attempt = manager.pending_peer_connection.as_mut().unwrap();
        tokio::time::timeout(Duration::from_secs(5), &mut attempt.task)
            .await
            .expect("seeding peer connection attempt timed out")
    };
    manager.finish_peer_connection_attempt(result);

    assert_eq!(manager.swarm.len(), 3);
    let peer_snapshots = manager.peer_snapshots();
    let expected_sources = [
        crate::request::request_group::BtPeerSource::Tracker,
        crate::request::request_group::BtPeerSource::Pex,
        crate::request::request_group::BtPeerSource::Dht,
    ];
    for (endpoint, expected_source) in endpoints.iter().zip(expected_sources) {
        assert!(
            peer_storage
                .lock()
                .unwrap()
                .get_peer(&endpoint.ip().to_string(), endpoint.port())
                .is_some_and(|peer| peer.is_active)
        );
        assert_eq!(
            peer_snapshots
                .iter()
                .find(|peer| peer.addr == *endpoint)
                .expect("discovered peer snapshot")
                .source,
            expected_source,
            "discovery provenance must survive peer storage and actor admission"
        );
    }

    manager.swarm.shutdown_all().await;
    for peer in peers {
        drop(peer.await.unwrap());
    }
}

#[tokio::test]
async fn slow_tracker_announce_does_not_block_seeding_peer_events() {
    let tracker_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tracker_address = tracker_listener.local_addr().unwrap();
    let response_body = b"d8:intervali60e5:peers0:e".to_vec();
    let (announce_started_tx, announce_started_rx) = tokio::sync::oneshot::channel();
    let (release_announce_tx, release_announce_rx) = tokio::sync::oneshot::channel();
    let tracker = tokio::spawn(async move {
        let mut announce_started_tx = Some(announce_started_tx);
        let mut release_announce_rx = Some(release_announce_rx);
        for request_index in 0..2 {
            let (mut socket, _) = tracker_listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let read = socket.read(&mut request).await.unwrap();
            assert!(read > 0);
            if request_index == 0 {
                announce_started_tx
                    .take()
                    .expect("initial announce notification sender missing")
                    .send(())
                    .unwrap();
                release_announce_rx
                    .take()
                    .expect("initial announce release receiver missing")
                    .await
                    .unwrap();
            }
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(&response_body).await.unwrap();
        }
    });

    let mut announcer = crate::engine::bittorrent::tracker::communication::TrackerAnnouncer::new(
        &[vec![format!("http://{tracker_address}/announce")]],
        &None,
    );
    announcer.set_timeouts(Duration::from_secs(3), Duration::from_secs(2));
    announcer.set_stopped_timeout(Duration::from_secs(1));

    let info_hash = [0x81; 20];
    let local_peer_id = [0x82; 20];
    let mut provider =
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1);
    provider.set_piece_data(0, vec![0x5a; 16]);
    let provider = Arc::new(provider);
    let (incoming_sender, incoming_receiver) = mpsc::channel(2);
    let mut manager = BtSeedManager::new_with_transports(
        info_hash,
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        Some(announcer),
        local_peer_id,
        Some(incoming_receiver),
    );
    let cancellation = manager.cancellation_token();
    let manager_task = tokio::spawn(async move { manager.run_seeding_loop().await });

    tokio::time::timeout(Duration::from_secs(2), announce_started_rx)
        .await
        .expect("seeding loop did not start the tracker announce")
        .expect("tracker request start notification was dropped");

    let peer_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let peer_address = peer_listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move { TcpStream::connect(peer_address).await.unwrap() });
    let (server_stream, endpoint) = peer_listener.accept().await.unwrap();
    incoming_sender
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection: PeerConnection::from_stream_with_peer(
                server_stream,
                [0x83; 20],
                false,
                false,
            ),
            endpoint,
        })
        .await
        .unwrap();

    let mut client = PeerConnection::from_stream_with_peer(
        client_task.await.unwrap(),
        local_peer_id,
        false,
        false,
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), client.read_message())
            .await
            .expect("peer availability was blocked by the tracker request")
            .unwrap(),
        Some(aria2_protocol::bittorrent::message::types::BtMessage::Bitfield { .. })
    ));

    release_announce_tx.send(()).unwrap();
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(3), manager_task)
        .await
        .expect("seeding shutdown did not finish after tracker response")
        .unwrap()
        .unwrap();
    tracker.await.unwrap();
}

#[tokio::test]
async fn incoming_seeding_actor_forwards_peer_dht_port_to_the_dht_engine() {
    let dht_engine = aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
        aria2_protocol::bittorrent::dht::engine::DhtEngineConfig {
            query_timeout: Duration::from_secs(3),
            ..aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local()
        },
    )
    .await
    .unwrap();
    let dht_probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let options = crate::request::request_group::DownloadOptions {
        enable_utp: false,
        bt_max_peers: 4,
        ..crate::request::request_group::DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(705),
            Vec::new(),
            options.clone(),
        ),
    ));
    let discovery = super::SeedPeerDiscovery {
        group,
        dht_engines: {
            let mut engines = crate::engine::bittorrent::dht::engine_set::DhtEngineSet::default();
            engines.insert(Arc::clone(&dht_engine));
            engines
        },
        dht_lookup: crate::engine::bittorrent::download::execute::DhtPeriodicLookup::new(),
        listen_port: 0,
        connection_options:
            crate::engine::bittorrent::peer::interaction::BtPeerConnectionOptions::from_download_options(
                &options, [0x86; 20],
            ),
        total_size: 16,
        utp_socket: None,
        outbound_network_policy: Arc::new(crate::network::OutboundNetworkPolicy::direct()),
        enable_peer_exchange: false,
    };
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1),
    );
    let (incoming_sender, incoming_receiver) = mpsc::channel(2);
    let mut manager = BtSeedManager::new_with_transports(
        [0x84; 20],
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        None,
        [0x85; 20],
        Some(incoming_receiver),
    )
    .with_peer_discovery(discovery);
    let cancellation = manager.cancellation_token();
    let manager_task = tokio::spawn(async move { manager.run_seeding_loop().await });

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = listener.local_addr().unwrap();
    let client_task = tokio::spawn(async move { TcpStream::connect(endpoint).await.unwrap() });
    let (server_stream, endpoint) = listener.accept().await.unwrap();
    incoming_sender
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection: PeerConnection::from_stream_with_peer(
                server_stream,
                [0x87; 20],
                false,
                false,
            ),
            endpoint,
        })
        .await
        .unwrap();
    let mut peer =
        PeerConnection::from_stream_with_peer(client_task.await.unwrap(), [0x85; 20], false, false);
    peer.send_message(
        &aria2_protocol::bittorrent::message::types::BtMessage::Port {
            port: dht_probe.local_addr().unwrap().port(),
        },
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if dht_engine.stats().await.pending_transactions > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("incoming seed actor did not forward the peer's DHT port");

    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(3), manager_task)
        .await
        .expect("seeding loop did not shut down")
        .unwrap()
        .unwrap();
    dht_engine.shutdown_async().await;
}

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

#[tokio::test]
async fn seeding_accepts_a_peer_after_download_has_no_initial_peers() {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
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
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection: peer_connection,
            endpoint,
        })
        .await
        .unwrap();

    manager.drain_incoming_peers().await;

    assert_eq!(manager.num_sessions(), 1);
}

#[tokio::test]
async fn cancelled_seeding_loop_future_preserves_incoming_peer_and_actor_channels() {
    let mut provider =
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1);
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
        assert_eq!(availability.first(), Some(&5));
        assert_eq!(availability.get(1), Some(&0x80));
        cancel_after_availability.cancel();
    });
    let (server_stream, endpoint) = listener.accept().await.unwrap();
    let peer_connection =
        PeerConnection::from_stream_with_peer(server_stream, [2u8; 20], false, false);
    sender
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection: peer_connection,
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
    let mut provider =
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16 * 1024, 1);
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

        let extension_handshake = read_bt_frame(&mut stream).await;
        assert_eq!(extension_handshake.first(), Some(&20));
        assert_eq!(extension_handshake.get(1), Some(&0));

        let availability = read_bt_frame(&mut stream).await;
        assert_eq!(
            availability,
            [14],
            "a peer advertising Fast Extension receives HaveAll for a complete seed"
        );

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
        aria2_protocol::bittorrent::peer::incoming::receive(server_stream, &[info_hash])
            .await
            .unwrap()
            .complete(local_peer_id, None, false)
            .await
            .unwrap();
    sender
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection,
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
        manager.pending_connections.is_empty(),
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
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
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
        .send(crate::engine::bittorrent::peer::listener::IncomingPeer {
            connection,
            endpoint,
        })
        .await
        .unwrap();
    drop(sender);

    manager.swarm.close_event_receiver();
    manager.drain_incoming_peers().await;

    assert!(manager.swarm.is_empty());
    assert!(manager.pending_connections.is_empty());
    drop(client.await.unwrap());
}

#[tokio::test]
async fn initial_seed_peer_is_not_kept_raw_when_actor_startup_is_closed() {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
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
    let connection = crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
        PeerConnection::from_stream_with_peer(server, [0x72u8; 20], false, false),
        endpoint,
    );
    manager.pending_connections.push(connection);
    manager.swarm.close_event_receiver();
    manager.swarm.close_event_sender();
    manager.cancellation_token().cancel();

    manager.run_seeding_loop().await.unwrap();

    assert!(manager.pending_connections.is_empty());
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
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(1024, 1),
    );
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

#[tokio::test]
async fn seeding_actor_sends_periodic_pex_with_recently_dropped_peers() {
    let live_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let live_remote_socket = tokio::net::TcpSocket::new_v4().unwrap();
    live_remote_socket
        .bind(SocketAddr::new(Ipv4Addr::new(127, 0, 0, 2).into(), 0))
        .unwrap();
    let live_remote_stream = live_remote_socket
        .connect(live_listener.local_addr().unwrap())
        .await
        .unwrap();
    let (live_local_stream, live_endpoint) = live_listener.accept().await.unwrap();
    let mut live_connection =
        crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(live_local_stream, [0x71; 20], false, false),
            live_endpoint,
        );
    live_connection.incoming = false;
    live_connection.allocate_session_resource(16, 1, 16);
    live_connection.register_peer_extension("ut_pex", 19);

    let dropped_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dropped_remote_socket = tokio::net::TcpSocket::new_v4().unwrap();
    dropped_remote_socket
        .bind(SocketAddr::new(Ipv4Addr::new(127, 0, 0, 3).into(), 0))
        .unwrap();
    let dropped_remote_stream = dropped_remote_socket
        .connect(dropped_listener.local_addr().unwrap())
        .await
        .unwrap();
    let (dropped_local_stream, dropped_endpoint) = dropped_listener.accept().await.unwrap();
    let mut dropped_connection =
        crate::engine::bittorrent::peer::connection::BtPeerConn::from_incoming_tcp(
            PeerConnection::from_stream_with_peer(dropped_local_stream, [0x72; 20], false, false),
            dropped_endpoint,
        );
    dropped_connection.incoming = false;

    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1),
    );
    let provider_dyn: Arc<dyn crate::engine::bittorrent::peer::upload_session::PieceDataProvider> =
        provider;
    let mut swarm = crate::engine::bittorrent::peer::message_handler::PeerSwarm::new(8);
    assert!(
        swarm
            .spawn_peer(live_connection, None, Arc::clone(&provider_dyn))
            .is_ok()
    );
    let dropped_actor_id = dropped_connection.actor_id;
    assert!(
        swarm
            .spawn_peer(dropped_connection, None, Arc::clone(&provider_dyn))
            .is_ok()
    );
    swarm.apply_event(&PeerEvent::GracefulDisconnected {
        actor_id: dropped_actor_id,
    });
    swarm.remove_dead().await;

    let options = crate::request::request_group::DownloadOptions {
        enable_peer_exchange: true,
        ..crate::request::request_group::DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(919),
            Vec::new(),
            options.clone(),
        ),
    ));
    let discovery = super::SeedPeerDiscovery {
        group,
        dht_engines: crate::engine::bittorrent::dht::engine_set::DhtEngineSet::default(),
        dht_lookup: crate::engine::bittorrent::download::execute::DhtPeriodicLookup::new(),
        listen_port: 0,
        connection_options:
            crate::engine::bittorrent::peer::interaction::BtPeerConnectionOptions::from_download_options(
                &options,
                [0x73; 20],
            ),
        total_size: 16,
        utp_socket: None,
        outbound_network_policy: Arc::new(crate::network::OutboundNetworkPolicy::direct()),
        enable_peer_exchange: true,
    };
    let manager = BtSeedManager::new_with_swarm(
        [0x74; 20],
        swarm,
        Arc::clone(&provider_dyn),
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        None,
        [0x75; 20],
        None,
        Arc::new(AtomicU64::new(0)),
        Instant::now() - crate::engine::bittorrent::download::execute::PEX_SEND_INTERVAL,
    )
    .with_peer_discovery(discovery);
    let cancel = manager.cancellation_token();
    let manager_task = tokio::spawn(async move {
        let mut manager = manager;
        manager.run_seeding_loop().await
    });

    let mut remote =
        PeerConnection::from_stream_with_peer(live_remote_stream, [0x76; 20], false, false);
    let received_pex = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let Some(message) = remote.read_message().await.unwrap() else {
                continue;
            };
            if let aria2_protocol::bittorrent::message::types::BtMessage::Extended {
                ext_id: 19,
                payload,
            } = message
            {
                break payload;
            }
        }
    })
    .await
    .expect("seeding coordinator should send its due PEX message");
    let aria2_protocol::bittorrent::extension::pex::PexMessage::Added { dropped, .. } =
        aria2_protocol::bittorrent::extension::pex::PexHandler::parse_pex_data(&received_pex)
            .unwrap()
    else {
        panic!("expected a BEP 11 added/dropped payload");
    };
    assert!(dropped.iter().any(|peer| {
        peer.ip == dropped_endpoint.ip().to_string() && peer.port == dropped_endpoint.port()
    }));

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(1), manager_task)
        .await
        .expect("seeding manager should shut down after cancellation")
        .unwrap()
        .unwrap();
    drop(dropped_remote_stream);
}
