use super::*;

#[test]
fn c_api_dispatches_bittorrent_listener_runtime_options() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut session = Aria2RustSession::new(Vec::new()).expect("session should start");
    let (command_tx, mut command_rx) = crate::engine::engine_command::channel();
    session.command_tx = command_tx;

    let (timeout_entry, timeout_name, timeout_value) =
        kv("bt-incoming-peer-handshake-timeout", "42");
    let (utp_entry, utp_name, utp_value) = kv("bt-utp-max-connections", "73");
    let (file_limit_entry, file_limit_name, file_limit_value) = kv("bt-max-open-files", "17");
    let (dht_limit_entry, dht_limit_name, dht_limit_value) = kv("dht-max-peer-info-hashes", "128");
    let entries = [timeout_entry, utp_entry, file_limit_entry, dht_limit_entry];
    let result =
        unsafe { aria2_rust_change_global_option(&mut session, entries.as_ptr(), entries.len()) };

    assert_eq!(result, 0);
    assert_eq!(
        session.runtime.block_on(
            session
                .config
                .get_global_i64("bt-incoming-peer-handshake-timeout")
        ),
        Some(42)
    );
    assert_eq!(
        session
            .runtime
            .block_on(session.config.get_global_i64("bt-utp-max-connections")),
        Some(73)
    );
    assert_eq!(
        session
            .runtime
            .block_on(session.config.get_global_i64("dht-max-peer-info-hashes")),
        Some(128)
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(
            crate::engine::engine_command::EngineCommand::SetIncomingPeerHandshakeTimeout {
                seconds: 42
            }
        )
    ));
    assert!(matches!(
        command_rx.try_recv(),
        Ok(crate::engine::engine_command::EngineCommand::SetBtMaxOpenFiles { max: 17 })
    ));
    let dht_update = command_rx.try_recv().expect("DHT update command expected");
    let crate::engine::engine_command::EngineCommand::SetDhtPeerStorageConfig { config } =
        dht_update
    else {
        panic!("DHT storage update command expected next");
    };
    assert_eq!(config.max_info_hashes, 128);
    assert_eq!(
        config.max_peers_per_info_hash,
        aria2_protocol::bittorrent::dht::DhtPeerStorageConfig::default().max_peers_per_info_hash
    );
    let expected_utp_options =
        aria2_protocol::bittorrent::utp::UtpSocketOptions::default().with_max_connections(73);
    assert!(matches!(
        command_rx.try_recv(),
        Ok(crate::engine::engine_command::EngineCommand::SetUtpSocketOptions { options })
            if options == expected_utp_options
    ));
    assert!(matches!(
        command_rx.try_recv(),
        Err(crate::engine::engine_command::EngineCommandTryRecvError::Empty)
    ));

    drop((
        timeout_name,
        timeout_value,
        utp_name,
        utp_value,
        file_limit_name,
        file_limit_value,
        dht_limit_name,
        dht_limit_value,
    ));
    session.finalize();
}
