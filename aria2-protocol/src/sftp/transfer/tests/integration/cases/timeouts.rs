use super::super::*;

#[tokio::test]
async fn stalled_sftp_init_response_is_bounded_by_read_timeout() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = StalledSubsystemServer;
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

    let read_timeout = Duration::from_millis(300);
    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(Duration::from_secs(1), read_timeout);
    let mut connection = SshConnection::connect(options)
        .await
        .expect("SSH authentication should finish before the subsystem request");

    let started_at = std::time::Instant::now();
    let result = SftpSession::open(&mut connection).await;
    let error = result.expect_err("stalled SFTP INIT response unexpectedly succeeded");
    assert!(
        matches!(&error, SftpSessionError::Timeout(timeout) if *timeout == read_timeout),
        "unexpected error: {error:?}"
    );
    assert!(started_at.elapsed() < Duration::from_secs(2));

    drop(connection);
    server_task.abort();
    let _ = server_task.await;
}

#[tokio::test]
async fn stalled_session_channel_open_is_bounded_by_connect_timeout() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = StalledChannelOpenServer;
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

    let connect_timeout = Duration::from_millis(300);
    let options = SshOptions::new("127.0.0.1", "test-user")
        .with_port(port)
        .with_password("test-password")
        .with_host_key_mode(HostKeyCheckingMode::Disable)
        .with_timeouts(connect_timeout, Duration::from_secs(1));
    let mut connection = SshConnection::connect(options)
        .await
        .expect("SSH authentication should finish before opening a channel");

    let started_at = std::time::Instant::now();
    let result = connection.open_sftp_channel().await;
    let error = result.expect_err("stalled channel open unexpectedly succeeded");
    assert!(
        matches!(&error, crate::sftp::connection::SshError::ConnectionLost { message } if message.contains("Opening SFTP channel")),
        "unexpected error: {error:?}"
    );
    assert!(started_at.elapsed() < Duration::from_secs(2));

    drop(connection);
    server_task.abort();
    let _ = server_task.await;
}
