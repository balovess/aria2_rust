use super::*;

#[tokio::test]
async fn state_changing_command_marks_dirty_and_persists() {
    use crate::request::request_group::{GroupId, RequestGroup};
    use crate::session::auto_save_coordinator::AutoSaveCoordinator;

    let man = Arc::new(RequestGroupMan::new());
    let dir = std::env::temp_dir();
    let path = dir.join(format!("test_engine_autosave_{}.sess", std::process::id()));
    let _ = tokio::fs::remove_file(&path).await;

    let auto_save = Arc::new(tokio::sync::Mutex::new(AutoSaveCoordinator::new(
        man.clone(),
        Some((path.clone(), Duration::from_millis(0))),
        None,
    )));
    let auto_save_dirty_signal = auto_save.lock().await.dirty_signal();

    // The auto-save must share the SAME group manager that the engine
    // commands mutate, otherwise it serializes a stale/empty snapshot.
    let mut ctx = EngineLoopContext {
        group_man: man,
        dns_cache: Arc::new(tokio::sync::Mutex::new(DnsCache::new())),
        auto_save: Some(auto_save.clone()),
        auto_save_dirty_signal: Some(auto_save_dirty_signal),
        event_hooks: Arc::new(DownloadEventHooks::new()),
        file_alloc_man: Arc::new(tokio::sync::RwLock::new(FileAllocationMan::new())),
        keep_alive: false,
        server_stat_man: Arc::new(ServerStatMan::new()),
        server_stat_max_age: Some(Duration::from_secs(24 * 60 * 60)),
        server_stat_save_path: None,
        server_stat_save_interval: None,
        server_stat_next_save: None,
        global_limiter: None,
        #[cfg(feature = "bittorrent")]
        public_tracker_catalog: Arc::new(
            aria2_protocol::bittorrent::tracker::public_list::PublicTrackerList::new(),
        ),
        #[cfg(feature = "bittorrent")]
        bt_registry: Arc::new(std::sync::RwLock::new(
            crate::engine::bt_registry::BtRegistry::new(),
        )),
        #[cfg(feature = "bittorrent")]
        bt_listener: Arc::new(crate::engine::bt_peer_listener::BtPeerListenerManager::new()),
        #[cfg(feature = "bittorrent")]
        lpd_manager: Arc::new(crate::engine::lpd_manager::LpdManager::new()),
    };

    // Send an AddDownload command through the engine-command channel.
    let (tx, rx) = mpsc::unbounded_channel();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(42),
        vec!["http://example.com/engine-autosave.bin".to_string()],
        DownloadOptions::default(),
    )));
    tx.send(EngineCommand::AddDownload { group }).unwrap();
    let mut rx = EngineCommandReceiver::from_unbounded(rx);

    let mut halt_requested = false;
    let mut force_halt_requested = false;
    let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
    // Hold the coordinator lock while the command mutates state. The
    // engine must retain the dirty notification instead of dropping it
    // because the autosave writer is busy.
    let auto_save_guard = auto_save.lock().await;
    process_engine_commands(
        &mut ctx,
        &mut rx,
        &mut Vec::new(),
        &mut halt_requested,
        &mut force_halt_requested,
        &completion_tx,
    )
    .await;
    drop(auto_save_guard);

    // The AddDownload command must have flipped the dirty flag.
    assert!(
        auto_save.lock().await.is_session_dirty(),
        "AddDownload should mark the session dirty"
    );

    // With interval=0 and dirty=true, save_if_dirty() writes the file.
    auto_save.lock().await.save_if_dirty().await;
    assert!(path.exists(), "save_if_dirty should write the session file");
    let content = tokio::fs::read_to_string(&path).await.unwrap();
    assert!(
        content.contains("http://example.com/engine-autosave.bin"),
        "session file should contain the added URI"
    );

    let _ = tokio::fs::remove_file(&path).await;
}

#[tokio::test]
async fn global_rate_limit_command_updates_shared_limiter_and_snapshot() {
    let mut ctx = test_ctx(false);
    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(EngineCommand::SetGlobalRateLimit {
        download_limit: Some(2_000),
        upload_limit: Some(1_000),
    })
    .unwrap();
    let mut rx = EngineCommandReceiver::from_unbounded(rx);

    let mut running_downloads = Vec::new();
    let mut halt_requested = false;
    let mut force_halt_requested = false;
    let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
    process_engine_commands(
        &mut ctx,
        &mut rx,
        &mut running_downloads,
        &mut halt_requested,
        &mut force_halt_requested,
        &completion_tx,
    )
    .await;

    let limiter = ctx
        .global_limiter
        .as_ref()
        .expect("runtime updates should create the shared limiter")
        .clone();
    let config = limiter.config().await;
    assert_eq!(config.download_rate(), Some(2_000));
    assert_eq!(config.upload_rate(), Some(1_000));

    let man = &ctx.group_man;
    assert_eq!(man.global_download_limit(), Some(2_000));
    assert_eq!(man.global_upload_limit(), Some(1_000));
}

#[tokio::test]
async fn external_task_completed_command_does_not_mark_session_dirty() {
    let mut ctx = test_ctx(false);
    let dirty = Arc::new(std::sync::atomic::AtomicBool::new(false));
    ctx.auto_save_dirty_signal = Some(Arc::clone(&dirty));

    let (tx, rx) = mpsc::unbounded_channel();
    tx.send(EngineCommand::TaskCompleted {
        gid: GroupId::new(1),
        result: TaskResult::Success,
    })
    .unwrap();
    let mut rx = EngineCommandReceiver::from_unbounded(rx);

    let mut halt_requested = false;
    let mut force_halt_requested = false;
    let (completion_tx, _completion_rx) = mpsc::unbounded_channel();
    process_engine_commands(
        &mut ctx,
        &mut rx,
        &mut Vec::new(),
        &mut halt_requested,
        &mut force_halt_requested,
        &completion_tx,
    )
    .await;

    assert!(!dirty.load(std::sync::atomic::Ordering::Acquire));
}
