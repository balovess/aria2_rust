#[test]
fn manager_lists_and_finds_live_handles_without_exposing_groups() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["http://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");

    let handles = manager.handles();
    assert_eq!(handles.len(), 1);
    assert_eq!(handles[0].gid(), handle.gid());
    assert!(manager.find(handle.gid()).is_some());
    assert!(manager.find(GroupId::new(0xdead_beef)).is_none());
}

#[test]
fn manager_exposes_batch_lifecycle_commands() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, mut command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);

    manager
        .pause_all()
        .expect("pause-all command should be accepted");
    assert!(matches!(
        command_receiver.try_recv(),
        Ok(EngineCommand::PauseAll)
    ));

    manager
        .force_pause_all()
        .expect("force-pause-all command should be accepted");
    assert!(matches!(
        command_receiver.try_recv(),
        Ok(EngineCommand::ForcePauseAll)
    ));

    manager
        .resume_all()
        .expect("resume-all command should be accepted");
    assert!(matches!(
        command_receiver.try_recv(),
        Ok(EngineCommand::UnpauseAll)
    ));

    manager
        .set_max_concurrent(3)
        .expect("max-concurrent command should be accepted");
    assert!(matches!(
        command_receiver.try_recv(),
        Ok(EngineCommand::SetMaxConcurrent { max: 3 })
    ));

    manager
        .set_global_rate_limit(Some(1024), None)
        .expect("global rate-limit command should be accepted");
    assert!(matches!(
        command_receiver.try_recv(),
        Ok(EngineCommand::SetGlobalRateLimit {
            download_limit: Some(1024),
            upload_limit: None,
        })
    ));
}

#[test]
fn manager_exposes_runtime_queries_and_stopped_results() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);

    assert_eq!(manager.max_concurrent(), 5);
    assert_eq!(manager.global_download_limit(), None);
    assert_eq!(manager.global_upload_limit(), None);
    assert_eq!(manager.stopped_results_len(), 0);

    let handle = manager
        .add_uri(
            vec!["https://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    group_man
        .remove_group(handle.gid())
        .expect("reserved download should be removable");

    assert_eq!(manager.stopped_results_len(), 1);
    assert_eq!(manager.stopped_results(0, 1)[0].gid, handle.gid());
    assert_eq!(manager.clear_completed().expect("clear should succeed"), 1);
    assert_eq!(manager.stopped_results_len(), 0);
}

#[test]
fn handle_exposes_file_snapshot_without_rpc_polling() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["https://example.test/file.zip".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");

    let files = handle.get_files().expect("live group snapshot");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "file.zip");
    let uris = handle.get_uris().expect("URI snapshot");
    assert_eq!(uris.len(), 1);
    assert_eq!(uris[0].uri, "https://example.test/file.zip");
}

#[tokio::test]
async fn handle_changes_uris_and_wakes_snapshot_observers() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["https://example.test/first".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let signal = group_man.activity_signal();
    let mut observed = signal.generation();
    let add_uris = vec!["https://example.test/second".to_string()];

    assert_eq!(
        handle
            .change_uris(1, &[], &add_uris, Some(0))
            .expect("URI change should succeed"),
        (0, 1)
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        signal.wait_for_change(&mut observed),
    )
    .await
    .expect("URI changes must wake snapshot observers");

    let files = handle.get_files().expect("live group snapshot");
    assert_eq!(files[0].uris[0].uri, add_uris[0]);
}

#[test]
fn handle_applies_runtime_options_through_manager_policy() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["https://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let mut changes = HashMap::new();
    changes.insert("dir".to_string(), serde_json::json!("reserved-dir"));

    handle
        .change_options(changes)
        .expect("runtime option change should succeed");

    let group = group_man.find_group(handle.gid()).expect("group exists");
    let runtime_options = group.recover().runtime_options();
    assert_eq!(
        runtime_options.get("dir"),
        Some(&serde_json::json!("reserved-dir"))
    );
    assert_eq!(
        handle
            .runtime_options()
            .expect("live runtime options")
            .get("dir"),
        Some(&serde_json::json!("reserved-dir"))
    );
    assert!(
        handle
            .pending_options()
            .expect("live pending options")
            .is_empty()
    );
}

#[test]
fn handle_changes_reserved_queue_position() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let first = manager
        .add_uri(
            vec!["https://example.test/first".to_string()],
            DownloadOptions::default(),
        )
        .expect("first download submission");
    let second = manager
        .add_uri(
            vec!["https://example.test/second".to_string()],
            DownloadOptions::default(),
        )
        .expect("second download submission");

    assert_eq!(
        second
            .change_position(0, PositionMode::SetFromStart)
            .expect("reserved position change"),
        0
    );
    assert!(matches!(
        first.change_position(-1, PositionMode::SetFromStart),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
}
