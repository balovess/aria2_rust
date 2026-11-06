#[test]
fn control_commands_reject_unknown_and_invalid_states_before_queueing() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, mut command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let unknown = manager.handle(GroupId::new(0xdead_beef));

    assert!(matches!(
        unknown.pause(),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
    assert!(matches!(
        unknown.force_pause(),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
    assert!(matches!(
        unknown.resume(),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
    assert!(matches!(
        unknown.remove(),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
    assert!(matches!(
        unknown.force_remove(),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
    assert!(matches!(
        command_receiver.try_recv(),
        Err(super::super::engine_command::EngineCommandTryRecvError::Empty)
    ));

    let live = manager
        .add_uri(
            vec!["https://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");
    assert!(matches!(
        live.resume(),
        Err(DownloadManagerError::State(Aria2Error::InvalidArgument(_)))
    ));
}

#[tokio::test]
async fn starts_with_request_group_manager_in_one_step() {
    let mut engine = DownloadEngine::new();
    engine.set_keep_alive(true);
    let handle = engine
        .start_with_request_group_man(Arc::new(RequestGroupMan::new()))
        .expect("engine should start with a request-group manager");

    assert!(handle.downloads().handles().is_empty());
    tokio::time::timeout(Duration::from_secs(1), handle.shutdown_and_wait())
        .await
        .expect("engine should stop promptly")
        .expect("engine should stop cleanly");
}

#[tokio::test]
async fn force_shutdown_and_wait_stops_keep_alive_engine() {
    let mut engine = DownloadEngine::new();
    engine.set_keep_alive(true);
    let handle = engine
        .start_with_request_group_man(Arc::new(RequestGroupMan::new()))
        .expect("engine should start with a request-group manager");

    tokio::time::timeout(Duration::from_secs(1), handle.force_shutdown_and_wait())
        .await
        .expect("force shutdown should stop promptly")
        .expect("force shutdown should complete cleanly");
}

#[test]
fn add_uri_registers_before_command_dispatch() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);
    let handle = manager
        .add_uri(
            vec!["http://example.test/file".to_string()],
            DownloadOptions::default(),
        )
        .expect("download submission");

    assert!(handle.status_snapshot().is_some());
    assert_eq!(group_man.count(), 1);
}

#[test]
fn add_uri_rejects_empty_input_before_registration() {
    let group_man = Arc::new(RequestGroupMan::new());
    let (command_sender, _command_receiver) = super::super::engine_command::channel();
    let manager = manager(Arc::clone(&group_man), command_sender);

    for uris in [Vec::new(), vec![String::new()], vec!["   ".to_string()]] {
        let result = manager.add_uri(uris, DownloadOptions::default());
        assert!(matches!(
            result,
            Err(DownloadManagerError::Preparation(
                Aria2Error::InvalidArgument(_)
            ))
        ));
    }

    assert_eq!(group_man.count(), 0);
}
