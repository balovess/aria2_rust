use super::*;

#[tokio::test]
async fn dropping_connection_requests_close_without_bounded_queue_capacity() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let (reply, _response) = oneshot::channel();
    command_tx
        .try_send(Command::Send {
            id: 7,
            bytes: vec![1],
            reply,
        })
        .unwrap();
    let (_receive_tx, receive_rx) = mpsc::channel(1);
    let (_state_tx, state_rx) = watch::channel(ConnectionState::Established);
    let close_requested = Arc::new(AtomicBool::new(false));
    let close_notify = Arc::new(Notify::new());
    let handle = UtpConnectionHandle {
        id: 7,
        command_tx,
        receive_rx,
        state_rx,
        close_requested: Arc::clone(&close_requested),
        close_notify: Arc::clone(&close_notify),
    };

    drop(handle);

    assert!(close_requested.load(Ordering::Acquire));
    assert!(matches!(
        command_rx.try_recv(),
        Ok(Command::Send { id: 7, .. })
    ));
    assert!(matches!(
        command_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Disconnected)
    ));
    tokio::time::timeout(Duration::from_millis(50), close_notify.notified())
        .await
        .expect("drop close notification must not depend on command-queue capacity");
}
