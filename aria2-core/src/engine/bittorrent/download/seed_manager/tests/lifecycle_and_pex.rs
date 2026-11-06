use super::*;

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
        utp_transport: None,
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
