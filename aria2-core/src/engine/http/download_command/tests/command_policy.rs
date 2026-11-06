use super::*;

#[test]
fn http2_session_pool_count_is_limited_by_max_connection_per_server() {
    let options = DownloadOptions {
        http_version: crate::http::HttpVersion::Http2,
        max_http2_sessions_per_server: Some(16),
        max_connection_per_server: Some(1),
        ..DownloadOptions::default()
    };
    assert_eq!(
        crate::engine::http::client_config::range_client_pool_count(&options),
        1
    );

    let options = DownloadOptions {
        http_version: crate::http::HttpVersion::Http2,
        max_http2_sessions_per_server: Some(2),
        max_connection_per_server: Some(4),
        ..DownloadOptions::default()
    };
    assert_eq!(
        crate::engine::http::client_config::range_client_pool_count(&options),
        2
    );

    assert_eq!(
        crate::engine::http::client_config::range_client_pool_count(&DownloadOptions::default()),
        crate::constants::DEFAULT_MAX_CONNECTION_PER_SERVER
    );
}

#[test]
fn http11_and_auto_pools_allow_the_configured_connection_ceiling() {
    for http_version in [
        crate::http::HttpVersion::Http11,
        crate::http::HttpVersion::Auto,
    ] {
        let options = DownloadOptions {
            http_version,
            max_connection_per_server: Some(7),
            max_http2_sessions_per_server: Some(2),
            ..DownloadOptions::default()
        };
        assert_eq!(
            crate::engine::http::client_config::range_client_pool_count(&options),
            7
        );
    }
}

#[test]
fn candidate_uris_follow_runtime_change_uri_updates() {
    let old_uri = "http://example.test/old";
    let new_uri = "http://example.test/new";
    let options = DownloadOptions::default();
    let group = std::sync::Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1_014),
        vec![old_uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group(group.clone(), old_uri, &options, None, None)
        .expect("HTTP command should be created");

    group
        .recover_mut()
        .change_uris(1, &[old_uri.to_string()], &[new_uri.to_string()], None)
        .expect("runtime URI update should succeed");

    assert_eq!(command.candidate_uris(), vec![new_uri.to_string()]);
}

#[test]
fn command_timeout_comes_from_download_options() {
    let options = DownloadOptions {
        timeout: Some(7),
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(1001),
        "http://example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("HTTP command should accept a valid URI");

    assert_eq!(
        Command::timeout(&command),
        Some(Duration::from_secs(7)),
        "timeout must be the configured I/O inactivity duration"
    );
}

#[test]
fn force_sequential_disables_concurrent_range_downloads() {
    let options = DownloadOptions {
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(1002),
        "http://example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("HTTP command should accept a valid URI");

    assert!(!command.should_use_concurrent(16 * 1024 * 1024, true, 4));
}

#[test]
fn in_memory_metadata_retry_classification_matches_http_contract() {
    use super::super::in_memory::should_retry_in_memory_error;

    let policy = RetryPolicy::new(2, 0);
    let server_error = |code| Aria2Error::Recoverable(RecoverableError::ServerError { code });

    assert!(should_retry_in_memory_error(
        &server_error(504),
        0,
        &policy,
        0,
        false,
    ));
    assert!(!should_retry_in_memory_error(
        &server_error(504),
        1,
        &policy,
        0,
        false,
    ));
    assert!(!should_retry_in_memory_error(
        &server_error(500),
        0,
        &policy,
        1,
        false,
    ));
    assert!(!should_retry_in_memory_error(
        &server_error(502),
        0,
        &policy,
        0,
        false,
    ));
    assert!(should_retry_in_memory_error(
        &server_error(502),
        0,
        &policy,
        1,
        false,
    ));
    assert!(should_retry_in_memory_error(
        &server_error(503),
        0,
        &policy,
        1,
        false,
    ));
    assert!(!should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: "HTTP error: 429".to_string(),
        }),
        0,
        &policy,
        1,
        false,
    ));
    assert!(should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "connection reset".to_string(),
        }),
        0,
        &policy,
        0,
        false,
    ));
    assert!(should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::Timeout),
        0,
        &policy,
        0,
        false,
    ));
    assert!(!should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
        0,
        &policy,
        0,
        false,
    ));
    assert!(should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
        0,
        &policy,
        0,
        true,
    ));
}

#[tokio::test]
async fn in_memory_http_records_each_payload_chunk_for_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let first_chunk = vec![b'A'; 16 * 1024];
    let second_chunk = vec![b'B'; 16 * 1024];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let first_chunk = first_chunk.clone();
        let second_chunk = second_chunk.clone();
        async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let bytes_read = stream.read(&mut request).await.unwrap();
            assert!(bytes_read > 0, "HTTP fixture should receive a request");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first_chunk.len() + second_chunk.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&first_chunk).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            stream.write_all(&second_chunk).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let url = format!("http://{address}/metadata.torrent");
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9001),
        vec![url.clone()],
        options.clone(),
    )));
    let mut command =
        DownloadCommand::new_with_group(Arc::clone(&group), &url, &options, None, None).unwrap();

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if group.recover().completed_length() >= first_chunk.len() as u64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("in-memory HTTP did not receive its first payload chunk");
    let first_activity = group.recover().last_network_activity();

    tokio::time::timeout(Duration::from_secs(5), command_task)
        .await
        .expect("in-memory HTTP command did not complete")
        .expect("in-memory HTTP command panicked")
        .expect("in-memory HTTP command failed");
    server.await.unwrap();

    assert!(
        group.recover().last_network_activity() > first_activity,
        "each non-empty in-memory HTTP chunk must refresh the inactivity clock"
    );
}

#[tokio::test]
async fn interface_binding_reaches_the_http_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("HTTP fixture should bind");
    let address = listener.local_addr().expect("HTTP fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("HTTP client should connect");
        let mut request = [0u8; 4096];
        let bytes = stream.read(&mut request).await.expect("read HTTP request");
        assert!(bytes > 0, "HTTP request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write HTTP response");
        peer.ip()
    });

    let url = format!("http://{address}/bound.bin");
    let options = DownloadOptions {
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9101),
        vec![url.clone()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_addresses_and_policy(
        group,
        &url,
        &options,
        None,
        None,
        None,
        Arc::new(OutboundNetworkPolicy::single(std::net::IpAddr::V4(
            std::net::Ipv4Addr::LOCALHOST,
        ))),
    )
    .expect("HTTP command should build with an interface binding");

    let response = command
        .client
        .get(&url)
        .send()
        .await
        .expect("bound HTTP client should connect");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        server.await.expect("HTTP fixture should finish"),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[test]
fn test_progress_channel_auto_created() {
    let cmd = DownloadCommand::new(
        GroupId::new(1),
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new should succeed with a valid HTTP URI");

    assert!(
        cmd.has_progress_sender(),
        "progress_sender should be Some after construction (auto-created)"
    );
    assert!(
        cmd.has_progress_receiver(),
        "progress_receiver should be Some after construction (held for lazy spawn)"
    );
    assert!(
        !cmd.has_progress_aggregator_handle(),
        "progress_aggregator_handle should be None until execute() spawns it"
    );
}

#[test]
fn primary_http_client_applies_custom_tls_configuration() {
    let directory = tempfile::tempdir().expect("create temporary TLS configuration directory");
    let ca_path = directory.path().join("ca.pem");
    std::fs::write(&ca_path, b"not a CA certificate").expect("write invalid CA fixture");

    let options = DownloadOptions {
        ca_certificate: Some(ca_path.to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };
    let error = match DownloadCommand::new(
        GroupId::new(3),
        "https://example.com/file.bin",
        &options,
        None,
        None,
    ) {
        Ok(_) => panic!("invalid custom CA configuration must reject the primary client"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("Invalid CA certificate"));
}

#[test]
fn primary_http_client_builds_with_certificate_verification_disabled() {
    let options = DownloadOptions {
        check_certificate: false,
        ..DownloadOptions::default()
    };

    DownloadCommand::new(
        GroupId::new(4),
        "https://example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("verification-disabled TLS configuration should build the client");
}

#[tokio::test]
async fn test_progress_updates_flow_through_channel() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    let group_clone = Arc::clone(&group);

    let mut cmd = DownloadCommand::new_with_group(
        group,
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    assert!(cmd.has_progress_sender());
    assert!(cmd.has_progress_receiver());

    cmd.spawn_progress_aggregator();
    assert!(cmd.has_progress_aggregator_handle());
    assert!(!cmd.has_progress_receiver());

    cmd.send_progress_update(ProgressUpdate {
        completed_bytes: 4096,
        download_speed: 0,
        upload_speed: 0,
    });

    cmd.drain_progress_aggregator().await;
    assert!(!cmd.has_progress_sender());
    assert!(!cmd.has_progress_aggregator_handle());

    let completed = { group_clone.recover().get_completed_length() };
    assert_eq!(
        completed, 4096,
        "aggregator should have applied the progress update to RequestGroup"
    );
}

/// Verify that check_cancelled() returns Ok(()) for a fresh group
/// (status = Waiting) and Err(DownloadFailed) after the group is
/// marked Removed.
#[tokio::test]
async fn test_check_cancelled_returns_ok_for_active_group() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(10),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));

    let cmd = DownloadCommand::new_with_group(
        group,
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    // Fresh group (Waiting status) -- not cancelled.
    assert!(
        cmd.check_cancelled().is_ok(),
        "check_cancelled() should return Ok for a fresh (non-removed) group"
    );
}

#[tokio::test]
async fn test_check_cancelled_returns_err_after_remove() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(11),
        vec!["http://example.com/file.bin".to_string()],
        DownloadOptions::default(),
    )));

    let cmd = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");

    // Simulate aria2.remove / aria2.forceRemove which calls
    // RequestGroupMan::remove_group -> group.remove().
    {
        let mut g = group.recover_mut();
        g.remove().unwrap();
    }

    let err = cmd
        .check_cancelled()
        .expect_err("check_cancelled() should return Err after the group is marked Removed");
    assert!(
        matches!(err, Aria2Error::DownloadFailed(_)),
        "expected DownloadFailed error, got {:?}",
        err
    );
}

#[tokio::test]
async fn test_retry_wait_is_interruptible_when_paused() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(12),
        vec!["http://example.com/metadata.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/metadata.torrent",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");
    group.recover_mut().pause().unwrap();

    let result = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        command.wait_for_retry(std::time::Duration::from_secs(5)),
    )
    .await
    .expect("paused retry wait should stop promptly");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[tokio::test]
async fn test_retry_wait_wakes_when_paused_after_wait_starts() {
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(13),
        vec!["http://example.com/metadata.torrent".to_string()],
        DownloadOptions::default(),
    )));
    let command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        "http://example.com/metadata.torrent",
        &DownloadOptions::default(),
        None,
        None,
    )
    .expect("DownloadCommand::new_with_group should succeed");
    let wait_task = tokio::spawn(async move {
        command
            .wait_for_retry(std::time::Duration::from_secs(5))
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    group.recover_mut().pause().unwrap();

    let result = tokio::time::timeout(std::time::Duration::from_millis(100), wait_task)
        .await
        .expect("pause should wake an active metadata retry wait")
        .expect("metadata retry wait task should not panic");
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}
