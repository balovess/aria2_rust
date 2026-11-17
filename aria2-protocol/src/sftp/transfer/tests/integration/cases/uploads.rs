use super::*;

#[tokio::test]
async fn upload_rejects_local_eof_before_the_observed_file_size() {
    let (close_seen_sender, close_seen_receiver) = oneshot::channel();
    let observed_packets = Arc::new(Mutex::new(Vec::new()));
    let mut server = TestSshServer {
        read_started: Arc::new(Mutex::new(None)),
        close_seen: Arc::new(Mutex::new(Some(close_seen_sender))),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size: 0,
        eof_on_read: false,
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
    let local_path = directory.path().join("shrinking-source.bin");
    tokio::fs::write(&local_path, vec![b'x'; (REMOTE_FILE_SIZE * 2) as usize])
        .await
        .unwrap();
    let callback_path = local_path.clone();
    let options = TransferOptions::default().with_progress_callback(move |transferred, _, _| {
        if transferred >= REMOTE_FILE_SIZE {
            std::fs::OpenOptions::new()
                .write(true)
                .open(&callback_path)
                .and_then(|file| file.set_len(transferred))
                .expect("truncate upload source after its initial size was observed");
        }
    });
    let connection_options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let mut connection = SshConnection::connect(connection_options)
        .await
        .expect("connect to upload fixture");
    let session = SftpSession::open(&mut connection)
        .await
        .expect("initialize upload fixture session");

    let result = SftpTransfer::new(&session)
        .upload(&local_path, "/remote/shrinking-source.bin", &options)
        .await;
    assert!(matches!(
        result,
        Err(TransferError::LocalIo { source, .. })
            if source.kind() == std::io::ErrorKind::UnexpectedEof
    ));
    timeout(Duration::from_secs(5), close_seen_receiver)
        .await
        .expect("SFTP client did not close the handle after local EOF")
        .expect("CLOSE signal was dropped");

    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn upload_resumes_without_truncating_the_existing_remote_prefix() {
    let observed_packets = Arc::new(Mutex::new(Vec::new()));
    let mut server = TestSshServer {
        read_started: Arc::new(Mutex::new(None)),
        close_seen: Arc::new(Mutex::new(None)),
        observed_packets: Arc::clone(&observed_packets),
        remote_file_size: REMOTE_FILE_SIZE,
        eof_on_read: false,
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
    let local_path = directory.path().join("upload.bin");
    tokio::fs::write(&local_path, vec![b'x'; REMOTE_FILE_SIZE as usize + 3])
        .await
        .unwrap();
    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let mut connection = SshConnection::connect(options)
        .await
        .expect("connect to upload fixture");
    let session = SftpSession::open(&mut connection)
        .await
        .expect("initialize upload fixture session");
    let progress = SftpTransfer::new(&session)
        .upload(
            &local_path,
            "/remote/upload.bin",
            &TransferOptions::default(),
        )
        .await
        .expect("resume upload without truncating the remote prefix");

    assert_eq!(progress.bytes_transferred, REMOTE_FILE_SIZE + 3);
    let packets = observed_packets.lock().await;
    let open_flags = packets.iter().find_map(|packet| match packet {
        SftpPacket::Open { flags, .. } => Some(*flags),
        _ => None,
    });
    assert!(
        open_flags.is_some_and(|flags| flags & SSH_FXF_TRUNC == 0),
        "resumed upload must not request remote truncation: {packets:?}"
    );
    let write_offsets = packets
        .iter()
        .filter_map(|packet| match packet {
            SftpPacket::Write { offset, .. } => Some(*offset),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(write_offsets, vec![REMOTE_FILE_SIZE]);
    drop(packets);

    drop(session);
    drop(connection);
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn upload_reports_remote_close_failure() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("upload-close-failure.bin");
    tokio::fs::write(&local_path, b"upload payload")
        .await
        .unwrap();
    let (connection, session, _, server_task) = connect_test_sftp_server(0, SSH_FX_FAILURE).await;

    let result = SftpTransfer::new(&session)
        .upload(
            &local_path,
            "/remote/upload.bin",
            &TransferOptions::default(),
        )
        .await;
    assert!(matches!(
        result,
        Err(TransferError::RemoteOperation { operation, .. })
            if operation == "close after upload"
    ));

    drop(session);
    let _ = connection.disconnect().await;
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn upload_restarts_when_remote_file_is_larger_than_source() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("short-source.bin");
    tokio::fs::write(&local_path, b"short source")
        .await
        .unwrap();
    let (connection, session, observed_packets, server_task) =
        connect_test_sftp_server(100, SSH_FX_OK).await;

    SftpTransfer::new(&session)
        .upload(
            &local_path,
            "/remote/longer.bin",
            &TransferOptions::default(),
        )
        .await
        .expect("oversized remote file should be replaced from offset zero");

    let packets = observed_packets.lock().await;
    let open_flags = packets.iter().find_map(|packet| match packet {
        SftpPacket::Open { flags, .. } => Some(*flags),
        _ => None,
    });
    assert!(open_flags.is_some_and(|flags| flags & SSH_FXF_TRUNC != 0));
    let write_offsets = packets
        .iter()
        .filter_map(|packet| match packet {
            SftpPacket::Write { offset, .. } => Some(*offset),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(write_offsets, vec![0]);
    drop(packets);

    drop(session);
    let _ = connection.disconnect().await;
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn upload_rejects_resume_offset_beyond_local_file() {
    let directory = tempfile::tempdir().unwrap();
    let local_path = directory.path().join("short-source.bin");
    tokio::fs::write(&local_path, b"short source")
        .await
        .unwrap();
    let (connection, session, observed_packets, server_task) =
        connect_test_sftp_server(0, SSH_FX_OK).await;

    let result = SftpTransfer::new(&session)
        .upload(
            &local_path,
            "/remote/resume.bin",
            &TransferOptions::default().with_resume(100),
        )
        .await;
    assert!(matches!(
        result,
        Err(TransferError::LocalIo { source, .. })
            if source.kind() == std::io::ErrorKind::InvalidInput
    ));
    assert!(observed_packets.lock().await.is_empty());

    drop(session);
    let _ = connection.disconnect().await;
    server_task.abort();
    let _ = server_task.await;
}
