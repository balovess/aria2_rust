#[tokio::test]
async fn handle_waits_for_terminal_state_without_polling() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let gid = group_man
        .add_group(
            vec!["http://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("group registration");
    let handle = manager.handle(gid);
    let waiter = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait().await }
    });

    tokio::task::yield_now().await;
    let group = group_man.find_group(gid).expect("group exists");
    group.recover().mark_complete();

    let result = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("wait must be event-driven")
        .expect("wait task must not panic")
        .expect("terminal result");
    assert_eq!(result.status, DownloadStatus::Complete);
}

#[tokio::test]
async fn handle_waits_for_requested_status_without_polling() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["http://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let waiter = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait_for_status(DownloadStatus::Paused).await }
    });

    tokio::task::yield_now().await;
    let group = group_man.find_group(handle.gid()).expect("group exists");
    group
        .recover_mut()
        .pause()
        .expect("pause transition should succeed");

    let result = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("status wait must be event-driven")
        .expect("status wait task must not panic")
        .expect("status transition should be observed");
    assert_eq!(result.status, DownloadStatus::Paused);
}

#[tokio::test]
async fn handle_waits_for_metadata_and_replays_a_late_subscription() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["magnet:?xt=urn:btih:example".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let event = crate::MetadataResolvedEvent::new(handle.gid(), vec![GroupId::new(0x42)]);
    let waiter = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait_for_metadata().await }
    });

    tokio::task::yield_now().await;
    manager.event_hooks.notify_metadata_resolved(event.clone());

    let resolved = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("metadata wait must be event-driven")
        .expect("metadata wait task must not panic")
        .expect("metadata event should be observed");
    assert_eq!(resolved, event);

    let replayed = handle
        .wait_for_metadata()
        .await
        .expect("recent metadata event should be replayed");
    assert_eq!(replayed, event);
}

#[tokio::test]
async fn metadata_wait_can_be_cancelled_without_changing_download_state() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["magnet:?xt=urn:btih:example".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = handle
        .wait_for_metadata_with_cancellation(&cancellation)
        .await;

    assert!(matches!(result, Err(DownloadManagerError::WaitCancelled)));
    assert!(handle.status_snapshot().is_some());
}

#[tokio::test]
async fn metadata_wait_timeout_does_not_change_download_state() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["magnet:?xt=urn:btih:example".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");

    let result = handle
        .wait_for_metadata_with_timeout(Duration::from_millis(1))
        .await;

    assert!(matches!(
        result,
        Err(DownloadManagerError::WaitTimeout {
            operation: "metadata resolution"
        })
    ));
    assert!(handle.status_snapshot().is_some());
}

#[tokio::test]
async fn metadata_wait_returns_when_resolution_fails() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["magnet:?xt=urn:btih:example".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let waiter = tokio::spawn({
        let handle = handle.clone();
        async move { handle.wait_for_metadata().await }
    });

    tokio::task::yield_now().await;
    group_man
        .find_group(handle.gid())
        .expect("group exists")
        .recover()
        .mark_error("no metadata peers".to_string());

    let result = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("metadata failure must wake the waiter")
        .expect("metadata waiter must not panic");
    assert!(matches!(
        result,
        Err(DownloadManagerError::MetadataResolutionFailed { status, message })
            if status == "error" && message == "no metadata peers"
    ));
}

#[tokio::test]
async fn handle_wait_can_be_cancelled_without_changing_download_state() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["http://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    let cancellation = CancellationToken::new();
    let waiter = tokio::spawn({
        let handle = handle.clone();
        let cancellation = cancellation.clone();
        async move { handle.wait_with_cancellation(&cancellation).await }
    });

    tokio::task::yield_now().await;
    cancellation.cancel();

    let result = tokio::time::timeout(Duration::from_secs(1), waiter)
        .await
        .expect("cancellation must wake the wait")
        .expect("wait task must not panic");
    assert!(matches!(result, Err(DownloadManagerError::WaitCancelled)));
    assert!(handle.status_snapshot().is_some());
}

#[tokio::test]
async fn unknown_handle_waits_fail_without_waiting_for_an_event() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager.handle(GroupId::new(0xdead_beef));

    let status_result = tokio::time::timeout(
        Duration::from_secs(1),
        handle.wait_for_status(DownloadStatus::Waiting),
    )
    .await
    .expect("unknown status wait must return promptly");
    assert!(matches!(
        status_result,
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));

    let terminal_result = tokio::time::timeout(Duration::from_secs(1), handle.wait())
        .await
        .expect("unknown terminal wait must return promptly");
    assert!(matches!(
        terminal_result,
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
}
