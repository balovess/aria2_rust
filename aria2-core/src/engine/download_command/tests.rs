use std::sync::Arc;

use crate::engine::command::{Command, ProgressUpdate};
use crate::engine::download_command::DownloadCommand;
use crate::engine::retry_policy::RetryPolicy;
use crate::error::{Aria2Error, RecoverableError};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

impl DownloadCommand {
    fn has_progress_sender(&self) -> bool {
        self.progress_sender.is_some()
    }

    fn has_progress_receiver(&self) -> bool {
        self.progress_receiver.is_some()
    }

    fn has_progress_aggregator_handle(&self) -> bool {
        self.progress_aggregator_handle.is_some()
    }

    fn send_progress_update(&self, update: ProgressUpdate) {
        if let Some(ref sender) = self.progress_sender {
            sender
                .try_send(update)
                .expect("progress test channel should accept the update");
        } else {
            panic!("test called send_progress_update but no sender is set");
        }
    }
}

#[test]
fn in_memory_metadata_retry_classification_matches_http_contract() {
    let policy = RetryPolicy::new(2, 0);
    let server_error = |code| Aria2Error::Recoverable(RecoverableError::ServerError { code });

    assert!(super::execute::should_retry_in_memory_error(
        &server_error(504),
        0,
        &policy,
        0,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &server_error(504),
        1,
        &policy,
        0,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &server_error(500),
        0,
        &policy,
        1,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &server_error(502),
        0,
        &policy,
        0,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &server_error(502),
        0,
        &policy,
        1,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &server_error(503),
        0,
        &policy,
        1,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::HttpProtocolError {
            message: "HTTP error: 429".to_string(),
        }),
        0,
        &policy,
        1,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure {
            message: "connection reset".to_string(),
        }),
        0,
        &policy,
        0,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::Timeout),
        0,
        &policy,
        0,
        false,
    ));
    assert!(!super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
        0,
        &policy,
        0,
        false,
    ));
    assert!(super::execute::should_retry_in_memory_error(
        &Aria2Error::Recoverable(RecoverableError::ResourceNotFound),
        0,
        &policy,
        0,
        true,
    ));
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

#[tokio::test]
async fn proxy_client_leaves_redirects_for_the_download_flow() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local proxy fixture");
    let proxy_addr = listener.local_addr().expect("read proxy address");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests_for_server = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for request_number in 1..=2 {
            let accepted = if request_number == 1 {
                Some(
                    listener
                        .accept()
                        .await
                        .expect("accept initial proxy request"),
                )
            } else {
                tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept())
                    .await
                    .ok()
                    .map(|result| result.expect("accept redirected proxy request"))
            };
            let Some((mut stream, _)) = accepted else {
                break;
            };
            requests_for_server.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut request = vec![0; 4096];
            let bytes = stream.read(&mut request).await.expect("read proxy request");
            assert!(bytes > 0, "proxy request should not be empty");
            let response = if request_number == 1 {
                b"HTTP/1.1 302 Found\r\nLocation: http://origin.example/redirect-target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            };
            stream
                .write_all(response)
                .await
                .expect("write proxy response");
        }
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(12),
        "http://origin.example/file.bin",
        &options,
        None,
        None,
    )
    .expect("create proxied download command");

    let response = command
        .client
        .get("http://origin.example/file.bin")
        .send()
        .await
        .expect("proxy should return the redirect response");
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "proxy redirects must be handled by SequentialDownloader so URI and retry state stay canonical"
    );

    server.await.expect("proxy fixture should finish");
}

#[tokio::test]
async fn authentication_retry_follows_redirect_and_preserves_protection_space() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth redirect fixture");
    let address = listener.local_addr().expect("read auth redirect address");
    let server = tokio::spawn(async move {
        for request_number in 1..=3 {
            let (mut stream, _) = listener.accept().await.expect("accept auth request");
            let mut request = vec![0; 4096];
            let bytes = stream.read(&mut request).await.expect("read auth request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let has_authorization = request.lines().any(|line| {
                line.to_ascii_lowercase()
                    .starts_with("authorization: basic ")
            });

            match request_number {
                1 => {
                    assert!(request.starts_with("GET /protected/file.bin HTTP/1.1\r\n"));
                    assert!(
                        !has_authorization,
                        "initial request must be unauthenticated"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth challenge");
                }
                2 => {
                    assert!(request.starts_with("GET /protected/file.bin HTTP/1.1\r\n"));
                    assert!(has_authorization, "auth retry must include credentials");
                    stream
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /protected/final.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth redirect");
                }
                3 => {
                    assert!(request.starts_with("GET /protected/final.bin HTTP/1.1\r\n"));
                    assert!(
                        has_authorization,
                        "same-host redirect must preserve the activated protection space"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nauth-redirect\n",
                        )
                        .await
                        .expect("write final authenticated response");
                }
                _ => unreachable!(),
            }
        }
    });

    let directory = tempfile::tempdir().expect("create auth redirect directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("password".to_string()),
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/protected/file.bin");
    let output = directory.path().join("auth-redirect.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(15),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("auth-redirect.bin"),
    )
    .expect("create auth redirect command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("auth redirect download should not hang")
        .expect("auth retry redirect should complete");
    assert_eq!(
        std::fs::read(&output).expect("read authenticated redirect output"),
        b"auth-redirect\n"
    );
    server.await.expect("auth redirect fixture should finish");
}

#[tokio::test]
async fn conditional_get_304_completes_without_location() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind conditional GET fixture");
    let address = listener.local_addr().expect("read conditional GET address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept conditional GET");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read conditional GET request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /cached.bin HTTP/1.1\r\n"));
        assert!(
            request
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("if-modified-since:")),
            "conditional GET must send If-Modified-Since: {request}"
        );
        stream
            .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
            .await
            .expect("write 304 response");
    });

    let directory = tempfile::tempdir().expect("create conditional GET directory");
    let output = directory.path().join("cached.bin");
    std::fs::write(&output, b"cached bytes").expect("create cached output");
    let options = DownloadOptions {
        allow_overwrite: true,
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/cached.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(13),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("cached.bin"),
    )
    .expect("create conditional GET command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("conditional GET should not hang")
        .expect("304 should complete the cached download");
    assert_eq!(
        std::fs::read(&output).expect("read cached output"),
        b"cached bytes"
    );

    server.await.expect("conditional GET fixture should finish");
}

#[tokio::test]
async fn unconditional_304_is_rejected_as_http_protocol_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unconditional 304 fixture");
    let address = listener
        .local_addr()
        .expect("read unconditional 304 address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept unconditional 304");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read unconditional 304 request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /cached.bin HTTP/1.1\r\n"));
        assert!(!request.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("if-modified-since:") || lower.starts_with("if-none-match:")
        }));
        stream
            .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
            .await
            .expect("write unconditional 304 response");
    });

    let directory = tempfile::tempdir().expect("create unconditional 304 directory");
    let output = directory.path().join("cached.bin");
    std::fs::write(&output, b"cached bytes").expect("create cached output");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        max_retries: 1,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/cached.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(14),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("cached.bin"),
    )
    .expect("create unconditional 304 command");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("unconditional 304 should not hang")
        .expect_err("unconditional 304 must be rejected");
    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
            if message.contains("304")
    ));
    assert_eq!(
        std::fs::read(&output).expect("read cached output"),
        b"cached bytes"
    );

    server
        .await
        .expect("unconditional 304 fixture should finish");
}
