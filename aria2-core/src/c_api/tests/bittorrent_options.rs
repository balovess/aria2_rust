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
    let entries = [timeout_entry, utp_entry, file_limit_entry];
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
    ));
    session.finalize();
}
