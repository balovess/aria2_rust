use super::*;

#[tokio::test]
async fn cancellation_closes_an_active_sftp_download() {
    let (read_started_sender, read_started_receiver) = oneshot::channel();
    let (close_seen_sender, close_seen_receiver) = oneshot::channel();
    let read_started = Arc::new(Mutex::new(Some(read_started_sender)));
    let close_seen = Arc::new(Mutex::new(Some(close_seen_sender)));
    let observed_packets = Arc::new(Mutex::new(Vec::new()));

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = TestSshServer {
        read_started: Arc::clone(&read_started),
        close_seen: Arc::clone(&close_seen),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size: REMOTE_FILE_SIZE,
        eof_on_read: false,
        stall_on_stat: false,
        close_status_code: SSH_FX_OK,
    };
    let mut rng = TestRng;
    let mut config = server::Config::default();
    config.keys.push(
        russh::keys::PrivateKey::random(&mut rng, russh::keys::ssh_key::Algorithm::Ed25519)
            .unwrap(),
    );
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    let config = Arc::new(config);

    let server_task = tokio::spawn(async move {
        let (socket, peer_addr) = listener.accept().await.unwrap();
        let handler = server.new_client(Some(peer_addr));
        let running = server::run_stream(config, socket, handler).await.unwrap();
        let _ = running.await;
    });

    let cancellation = CancellationToken::new();
    let client_cancellation = cancellation.clone();
    let local_path = std::path::PathBuf::from("target")
        .join(format!("sftp-cancellation-{}.bin", std::process::id()));
    tokio::fs::create_dir_all("target")
        .await
        .expect("failed to create the Cargo test output directory");
    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));

    let client_local_path = local_path.clone();
    let client_task = tokio::spawn(async move {
        let mut connection = SshConnection::connect(options)
            .await
            .expect("failed to connect to in-process SFTP server");
        let session = SftpSession::open(&mut connection)
            .await
            .expect("failed to initialize the in-process SFTP session");
        SftpTransfer::new(&session)
            .download_with_cancellation(
                "/remote/cancellation.bin",
                &client_local_path,
                &TransferOptions::default(),
                &client_cancellation,
            )
            .await
    });

    timeout(Duration::from_secs(5), read_started_receiver)
        .await
        .expect("SFTP server did not reach the active READ")
        .expect("SFTP server readiness signal was dropped");
    cancellation.cancel();

    let result = timeout(Duration::from_secs(5), client_task)
        .await
        .expect("cancellable SFTP download did not finish")
        .expect("cancellable SFTP client task panicked");
    assert!(matches!(result, Err(TransferError::Cancelled)));

    timeout(Duration::from_secs(5), close_seen_receiver)
        .await
        .expect("SFTP client did not close the remote handle")
        .expect("SFTP close signal was dropped");

    let _ = tokio::fs::remove_file(&local_path).await;
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn cancelled_in_flight_request_poison_session_before_next_exchange() {
    let (request_started_sender, request_started_receiver) = oneshot::channel();
    let observed_packets = Arc::new(Mutex::new(Vec::new()));
    let mut server = TestSshServer {
        read_started: Arc::new(Mutex::new(Some(request_started_sender))),
        close_seen: Arc::new(Mutex::new(None)),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size: REMOTE_FILE_SIZE,
        eof_on_read: false,
        stall_on_stat: true,
        close_status_code: SSH_FX_OK,
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut rng = TestRng;
    let mut config = server::Config::default();
    config.keys.push(
        russh::keys::PrivateKey::random(&mut rng, russh::keys::ssh_key::Algorithm::Ed25519)
            .unwrap(),
    );
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    let config = Arc::new(config);
    let server_task = tokio::spawn(async move {
        let (socket, peer_addr) = listener.accept().await.unwrap();
        let handler = server.new_client(Some(peer_addr));
        let running = server::run_stream(config, socket, handler).await.unwrap();
        let _ = running.await;
    });

    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let mut connection = SshConnection::connect(options)
        .await
        .expect("connect to stalled SFTP request fixture");
    let session = SftpSession::open(&mut connection)
        .await
        .expect("initialize stalled SFTP request fixture");
    let request_session = session.clone();
    let request = tokio::spawn(async move {
        crate::sftp::file_ops::SftpFileOps::new(&request_session)
            .stat("/stalled")
            .await
    });

    timeout(Duration::from_secs(5), request_started_receiver)
        .await
        .expect("server did not receive the SFTP request")
        .expect("server request signal was dropped");
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());

    let second_result = crate::sftp::file_ops::SftpFileOps::new(&session)
        .stat("/must-not-be-sent")
        .await;
    assert!(matches!(
        second_result,
        Err(crate::sftp::file_ops::FileOpError::Session {
            source: SftpSessionError::SessionPoisoned,
            ..
        })
    ));
    assert_eq!(
        observed_packets.lock().await.as_slice(),
        &[SftpPacket::Stat {
            request_id: 1,
            path: "/stalled".into(),
        }]
    );

    connection
        .disconnect()
        .await
        .expect("disconnect poisoned SFTP connection");
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn download_rejects_eof_before_the_remote_file_size() {
    let (read_started_sender, read_started_receiver) = oneshot::channel();
    let (close_seen_sender, close_seen_receiver) = oneshot::channel();
    let observed_packets = Arc::new(Mutex::new(Vec::new()));
    let mut server = TestSshServer {
        read_started: Arc::new(Mutex::new(Some(read_started_sender))),
        close_seen: Arc::new(Mutex::new(Some(close_seen_sender))),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size: REMOTE_FILE_SIZE,
        eof_on_read: true,
        stall_on_stat: false,
        close_status_code: SSH_FX_OK,
    };
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut rng = TestRng;
    let mut config = server::Config::default();
    config.keys.push(
        russh::keys::PrivateKey::random(&mut rng, russh::keys::ssh_key::Algorithm::Ed25519)
            .unwrap(),
    );
    config.auth_rejection_time_initial = Some(Duration::from_millis(1));
    let config = Arc::new(config);
    let server_task = tokio::spawn(async move {
        let (socket, peer_addr) = listener.accept().await.unwrap();
        let handler = server.new_client(Some(peer_addr));
        let running = server::run_stream(config, socket, handler).await.unwrap();
        let _ = running.await;
    });

    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("early-eof.bin");
    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let mut connection = SshConnection::connect(options)
        .await
        .expect("connect to EOF fixture");
    let session = SftpSession::open(&mut connection)
        .await
        .expect("initialize EOF fixture session");

    let result = SftpTransfer::new(&session)
        .download(
            "/remote/early-eof.bin",
            &local_path,
            &TransferOptions::default(),
        )
        .await;
    assert!(matches!(
        result,
        Err(TransferError::RemoteOperation {
            source: crate::sftp::file_ops::FileOpError::Other { message },
            ..
        }) if message.contains("unexpected EOF")
    ));
    timeout(Duration::from_secs(5), read_started_receiver)
        .await
        .expect("SFTP server did not receive READ")
        .expect("READ signal was dropped");
    timeout(Duration::from_secs(5), close_seen_receiver)
        .await
        .expect("SFTP client did not close the handle after early EOF")
        .expect("CLOSE signal was dropped");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn download_keeps_complete_payload_when_remote_close_fails() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("complete-download.bin");
    let payload = b"partial-data";
    let (connection, session, _, server_task) =
        connect_test_sftp_server(payload.len() as u64, SSH_FX_FAILURE).await;

    SftpTransfer::new(&session)
        .download(
            "/remote/complete.bin",
            &local_path,
            &TransferOptions::default(),
        )
        .await
        .expect("remote handle cleanup failure must not discard complete download");
    assert_eq!(tokio::fs::read(&local_path).await.unwrap(), payload);

    drop(session);
    let _ = connection.disconnect().await;
    server_task.abort();
    let _ = server_task.await;
}
