use super::*;

#[test]
fn public_seed_manager_keeps_its_independent_tracker_announcer_accessible() {
    let provider = Arc::new(
        crate::engine::bittorrent::peer::upload_session::InMemoryPieceProvider::new(16, 1),
    );
    let mut manager = BtSeedManager::new_with_announcer(
        [0x71; 20],
        Vec::new(),
        provider,
        BtSeedingConfig::default(),
        SeedExitCondition::infinite(),
        16,
        Some(TrackerAnnouncer::new(&[], &None)),
        [0x72; 20],
    );

    assert!(manager.take_announcer().is_some());
}

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
        utp_transport: None,
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
            connection: crate::engine::bittorrent::peer::listener::IncomingPeerConnection::Tcp(
                PeerConnection::from_stream_with_peer(server_stream, [0x83; 20], false, false),
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
        utp_transport: None,
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
            connection: crate::engine::bittorrent::peer::listener::IncomingPeerConnection::Tcp(
                PeerConnection::from_stream_with_peer(server_stream, [0x87; 20], false, false),
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
