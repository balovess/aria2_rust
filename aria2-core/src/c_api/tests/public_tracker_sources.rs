#[cfg(feature = "bittorrent")]
use super::*;
#[cfg(feature = "bittorrent")]
use std::ptr;

#[cfg(feature = "bittorrent")]
#[test]
fn c_api_accepts_an_empty_public_tracker_source_list() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let (entry, name, value) = kv("bt-tracker-source", "");
    assert_eq!(
        unsafe { aria2_rust_change_global_option(session, &entry, 1) },
        0
    );

    let session_ref = unsafe { &*session };
    assert_eq!(
        session_ref
            .runtime
            .block_on(session_ref.config.get_global_option("bt-tracker-source")),
        Some(OptionValue::List(Vec::new()))
    );

    drop((name, value));
    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[cfg(feature = "bittorrent")]
#[test]
fn c_api_dispatches_public_tracker_source_concurrency() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut session = Aria2RustSession::new(Vec::new()).expect("session should start");
    let (command_tx, mut command_rx) = crate::engine::engine_command::channel();
    session.command_tx = command_tx;
    let (entry, name, value) = kv("bt-tracker-source-concurrency", "2");

    assert_eq!(
        unsafe { aria2_rust_change_global_option(&mut session, &entry, 1) },
        0
    );
    assert_eq!(
        session.runtime.block_on(
            session
                .config
                .get_global_i64("bt-tracker-source-concurrency")
        ),
        Some(2)
    );
    assert!(matches!(
        command_rx.try_recv(),
        Ok(
            crate::engine::engine_command::EngineCommand::SetPublicTrackerSourceConcurrency {
                max_concurrent_sources: 2
            }
        )
    ));

    drop((name, value));
    session.finalize();
}
