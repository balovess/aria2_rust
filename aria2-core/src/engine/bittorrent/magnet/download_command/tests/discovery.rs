use super::*;

#[test]
fn magnet_metadata_discovery_uses_embedded_and_configured_trackers() {
    let magnet = aria2_protocol::bittorrent::magnet::MagnetLink::parse(
            "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&tr=udp%3A%2F%2Fembedded.test%3A6969&tr=http%3A%2F%2Fexcluded.test%2Fannounce&tr=wss%3A%2F%2Ftracker.example%2Fannounce",
        )
        .expect("test magnet should parse");
    let options = DownloadOptions {
        bt_tracker: Some(vec![
            "https://configured.test/announce".to_string(),
            "udp://embedded.test:6969".to_string(),
        ]),
        bt_exclude_tracker: Some(vec!["http://excluded.test/announce".to_string()]),
        ..DownloadOptions::default()
    };

    assert_eq!(
        metadata_tracker_urls(&magnet, &options),
        vec![
            "udp://embedded.test:6969".to_string(),
            "wss://tracker.example/announce".to_string(),
            "https://configured.test/announce".to_string(),
        ]
    );
}

#[test]
fn tracker_peer_addresses_accept_ipv4_and_ipv6() {
    assert_eq!(
        tracker_peer_socket_addr("192.0.2.10", 6881),
        Some("192.0.2.10:6881".parse().unwrap())
    );
    assert_eq!(
        tracker_peer_socket_addr("2001:db8::10", 6881),
        Some("[2001:db8::10]:6881".parse().unwrap())
    );
    assert!(tracker_peer_socket_addr("not-an-ip", 6881).is_none());
}

#[test]
fn tracker_peer_collection_deduplicates_and_bounds_results() {
    let mut discovered = Vec::new();
    let peers = (0..(MAX_MAGNET_TRACKER_PEERS + 5))
        .map(|offset| SocketAddr::from(([192, 0, 2, 1], 10_000 + offset as u16)))
        .collect();

    append_unique_tracker_peers(&mut discovered, peers);
    assert_eq!(discovered.len(), MAX_MAGNET_TRACKER_PEERS);

    let duplicate = discovered[0];
    append_unique_tracker_peers(&mut discovered, vec![duplicate]);
    assert_eq!(discovered.len(), MAX_MAGNET_TRACKER_PEERS);
}

/// Start a real DhtEngine on an ephemeral port for testing.
///
/// Uses `DhtEngineConfig::local()`: an OS-assigned port (avoids conflicts)
/// and no public bootstrap, so the test performs no outbound network I/O
/// and cannot stall on DNS or unreachable entry points.
async fn start_test_dht_engine()
-> std::sync::Arc<aria2_protocol::bittorrent::dht::engine::DhtEngine> {
    aria2_protocol::bittorrent::dht::engine::DhtEngine::start(
        aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
    )
    .await
    .expect("Failed to start DhtEngine for test")
}

/// BEP 0027: After metadata exchange, a private torrent must cause the
/// DHT engine (started for peer discovery) to be shut down and the
/// family-scoped `dht_engines` set emptied so the downstream `BtDownloadCommand`
/// cannot accidentally reuse it.

#[tokio::test]
async fn test_magnet_private_torrent_dht_shutdown_after_metadata() {
    let mut cmd = make_test_command();
    cmd.dht_engines.insert(start_test_dht_engine().await);
    assert!(
        !cmd.dht_engines.is_empty(),
        "precondition: DHT engine present"
    );

    let torrent_bytes = build_private_test_torrent();

    cmd.enforce_bep0027_after_metadata(&torrent_bytes)
        .await
        .expect("enforce_bep0027 should succeed for private torrent");

    assert!(
        cmd.dht_engines.is_empty(),
        "DHT engines must be empty after private torrent metadata (BEP 0027)"
    );
}

/// BEP 0027: A public torrent (no `private` flag) must NOT trigger DHT
/// shutdown — the DHT engine started for peer discovery remains active
/// so the downstream download can continue using it.

#[tokio::test]
async fn test_magnet_public_torrent_dht_continues() {
    let mut cmd = make_test_command();
    let engine = start_test_dht_engine().await;
    cmd.dht_engines.insert(Arc::clone(&engine));

    let torrent_bytes = build_test_torrent();

    cmd.enforce_bep0027_after_metadata(&torrent_bytes)
        .await
        .expect("enforce_bep0027 should succeed for public torrent");

    assert!(
        !cmd.dht_engines.is_empty(),
        "DHT engine must remain active for public torrent"
    );

    // Clean up: shut down the still-running engine to release the socket.
    engine.shutdown_async().await;
}

#[tokio::test]
async fn private_magnet_does_not_shutdown_a_shared_global_dht_engine() {
    let mut cmd = make_test_command();
    let registry = std::sync::Arc::new(std::sync::RwLock::new(
        crate::engine::bittorrent::registry::BtRegistry::new(),
    ));
    let engine = start_test_dht_engine().await;
    registry
        .write()
        .expect("BT registry should be writable")
        .set_global_dht_engine(std::sync::Arc::clone(&engine));
    cmd.set_bt_registry(std::sync::Arc::clone(&registry));
    let gid = cmd.group().gid().value();
    assert!(
        cmd.dht_engines
            .attach_global_if_present(Some(&registry), gid, false,)
    );

    cmd.enforce_bep0027_after_metadata(&build_private_test_torrent())
        .await
        .expect("private metadata should be accepted");

    assert!(cmd.dht_engines.is_empty());
    assert_eq!(
        engine.state().await,
        aria2_protocol::bittorrent::dht::engine::DhtEngineState::Running
    );
    assert!(
        registry
            .read()
            .expect("BT registry should be readable")
            .get_global_dht_engine_for_peer(engine.local_addr())
            .is_some_and(|global| std::sync::Arc::ptr_eq(&global, &engine))
    );

    engine.shutdown_async().await;
}

#[tokio::test]
async fn public_magnet_hands_its_dht_engine_to_the_payload_command() {
    use crate::engine::bittorrent::download::command::BtDownloadCommand;

    let mut cmd = make_test_command();
    let registry = std::sync::Arc::new(std::sync::RwLock::new(
        crate::engine::bittorrent::registry::BtRegistry::new(),
    ));
    cmd.set_bt_registry(std::sync::Arc::clone(&registry));
    let gid = cmd.group().gid().value();
    cmd.dht_engines
        .start_or_join(
            Some(&registry),
            gid,
            aria2_protocol::bittorrent::dht::engine::DhtEngineConfig::local(),
        )
        .await
        .expect("magnet should start and register its shared DHT engine");
    let engine = cmd
        .dht_engines
        .ipv4()
        .expect("magnet should retain the IPv4 DHT handle");

    let torrent = build_test_torrent();
    let options = DownloadOptions::default();
    let mut payload = BtDownloadCommand::new(GroupId::new(1), &torrent, &options, None)
        .expect("public torrent should construct");
    payload.set_bt_registry(std::sync::Arc::clone(&registry));

    cmd.handoff_dht_engine_to_bt(&mut payload).await;

    let transferred = payload
        .dht_engines
        .ipv4()
        .expect("payload command should receive the DHT engine");
    assert!(std::sync::Arc::ptr_eq(&transferred, &engine));
    assert!(cmd.dht_engines.is_empty());
    assert!(
        registry
            .read()
            .expect("BT registry should be readable")
            .get_global_dht_engine_for_peer(engine.local_addr())
            .is_some_and(|global| std::sync::Arc::ptr_eq(&global, &engine))
    );

    engine.shutdown_async().await;
}

#[tokio::test]
async fn public_magnet_prefers_an_existing_global_dht_engine_at_handoff() {
    use crate::engine::bittorrent::download::command::BtDownloadCommand;

    let mut cmd = make_test_command();
    let registry = std::sync::Arc::new(std::sync::RwLock::new(
        crate::engine::bittorrent::registry::BtRegistry::new(),
    ));
    let global_engine = start_test_dht_engine().await;
    registry
        .write()
        .expect("BT registry should be writable")
        .set_global_dht_engine(std::sync::Arc::clone(&global_engine));
    cmd.set_bt_registry(std::sync::Arc::clone(&registry));
    cmd.ensure_dht_engines(&DownloadOptions {
        enable_dht: true,
        ..DownloadOptions::default()
    })
    .await
    .expect("magnet should attach to the existing family engine");
    assert!(
        cmd.dht_engines
            .ipv4()
            .as_ref()
            .is_some_and(|engine| std::sync::Arc::ptr_eq(engine, &global_engine))
    );

    let torrent = build_test_torrent();
    let options = DownloadOptions::default();
    let mut payload = BtDownloadCommand::new(GroupId::new(1), &torrent, &options, None)
        .expect("public torrent should construct");
    payload.set_bt_registry(std::sync::Arc::clone(&registry));

    cmd.handoff_dht_engine_to_bt(&mut payload).await;

    assert_eq!(
        global_engine.state().await,
        aria2_protocol::bittorrent::dht::engine::DhtEngineState::Running
    );
    assert!(
        payload
            .dht_engines
            .ipv4()
            .as_ref()
            .is_some_and(|engine| std::sync::Arc::ptr_eq(engine, &global_engine))
    );

    global_engine.shutdown_async().await;
}
