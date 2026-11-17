use super::*;

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
                            b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nContent-Disposition: attachment; filename=authenticated.bin\r\nConnection: close\r\n\r\nauth-redirect\n",
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
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/protected/file.bin");
    let output = directory.path().join("authenticated.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(15),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
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
async fn metadata_get_challenge_is_reused_by_the_range_probe() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind metadata auth fixture");
    let address = listener.local_addr().expect("metadata auth address");
    let server = tokio::spawn(async move {
        let (mut unauthenticated, _) = listener.accept().await.expect("accept metadata GET");
        let request = read_http_request(&mut unauthenticated).await;
        assert!(request.starts_with("GET /metadata.bin HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        unauthenticated
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write metadata auth challenge");

        let (mut authenticated, _) = listener.accept().await.expect("accept metadata retry");
        let request = read_http_request(&mut authenticated).await;
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        authenticated
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated metadata response");

        let (mut range, _) = listener.accept().await.expect("accept Range probe");
        let request = read_http_request(&mut range).await;
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("range") && value.trim() == "bytes=0-0"
            })
        }));
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        range
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/10\r\nContent-Length: 1\r\nConnection: close\r\n\r\n0",
            )
            .await
            .expect("write authenticated Range response");
    });

    let directory = tempfile::tempdir().expect("create metadata auth directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        http_auth_challenge: true,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/metadata.bin");
    let command = DownloadCommand::new(
        GroupId::new(152),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("metadata.bin"),
    )
    .expect("create metadata auth command");
    let prepared = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        command.send_get_with_redirects(&uri),
    )
    .await
    .expect("metadata GET should not hang")
    .expect("metadata GET should return a response");

    let auth_options = crate::http::AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..crate::http::AuthResolveOptions::default()
    };
    let prober =
        crate::engine::http::segment_downloader::HttpSegmentDownloader::new(&command.client)
            .with_shared_auth_options(auth_options, prepared.auth_factory);
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        prober.probe_range_metadata(&uri, None),
    )
    .await
    .expect("Range probe should not hang")
    .expect("Range probe should reuse the activated metadata credentials");

    assert!(probe.supports_range);
    server.await.expect("metadata auth fixture should finish");
}

#[tokio::test]
async fn metadata_get_challenge_is_reused_when_sequential_download_reissues_get() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sequential metadata auth fixture");
    let address = listener
        .local_addr()
        .expect("sequential metadata auth address");
    let server = tokio::spawn(async move {
        let (mut unauthenticated, _) = listener.accept().await.expect("accept metadata GET");
        let mut request = vec![0; 4096];
        let bytes = unauthenticated
            .read(&mut request)
            .await
            .expect("read metadata GET");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /sequential.bin HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        unauthenticated
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write metadata auth challenge");

        let (mut authenticated, _) = listener.accept().await.expect("accept metadata retry");
        let mut request = vec![0; 4096];
        let bytes = authenticated
            .read(&mut request)
            .await
            .expect("read metadata auth retry");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        authenticated
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated metadata response");

        let (mut sequential, _) = listener.accept().await.expect("accept sequential GET");
        let mut request = vec![0; 4096];
        let bytes = sequential
            .read(&mut request)
            .await
            .expect("read sequential GET");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /sequential.bin HTTP/1.1\r\n"));
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        sequential
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write sequential response");
    });

    let directory = tempfile::tempdir().expect("create sequential auth directory");
    let output = directory.path().join("sequential.bin");
    let options = DownloadOptions {
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        http_auth_challenge: true,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/sequential.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(153),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
    )
    .expect("create sequential metadata auth command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("sequential metadata auth download should not hang")
        .expect("sequential metadata auth download should complete");
    assert_eq!(
        std::fs::read(&output).expect("read sequential auth output"),
        b"0123456789"
    );
    server.await.expect("sequential auth fixture should finish");
}

#[tokio::test]
async fn sequential_transfer_failure_retry_reuses_activated_basic_credentials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sequential retry auth fixture");
    let address = listener
        .local_addr()
        .expect("read sequential retry auth address");
    let server = tokio::spawn(async move {
        for request_number in 1..=4 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept sequential retry request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let count = stream
                    .read(&mut chunk)
                    .await
                    .expect("read sequential retry request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("request headers should be UTF-8");
            assert!(request.starts_with("GET /retry.bin HTTP/1.1\r\n"));
            let has_basic_auth = request.lines().any(|line| {
                line.split_once(':').is_some_and(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization")
                        && value.trim() == "Basic dXNlcjpwYXNz"
                })
            });

            match request_number {
                1 => {
                    assert!(
                        !has_basic_auth,
                        "first request should challenge credentials"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"retry\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write Basic challenge");
                }
                2 => {
                    assert!(has_basic_auth, "metadata auth retry must be authenticated");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                        )
                        .await
                        .expect("write authenticated metadata response");
                }
                3 => {
                    assert!(has_basic_auth, "first transfer request must retain auth");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123",
                        )
                        .await
                        .expect("write intentionally truncated transfer response");
                }
                4 => {
                    assert!(
                        has_basic_auth,
                        "transfer-failure retry must reuse the activated credentials: {request}"
                    );
                    let range_start = request.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        let range = if name.eq_ignore_ascii_case("range") {
                            value.trim().strip_prefix("bytes=")
                        } else {
                            None
                        }?;
                        range
                            .split_once('-')
                            .and_then(|(start, _)| start.parse::<usize>().ok())
                    });
                    if let Some(start) = range_start.filter(|start| *start < 10) {
                        let body = &b"0123456789"[start..];
                        stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-9/10\r\nConnection: close\r\n\r\n",
                                    body.len(),
                                    start
                                )
                                .as_bytes(),
                            )
                            .await
                            .expect("write retry range response headers");
                        stream
                            .write_all(body)
                            .await
                            .expect("write retry range response body");
                    } else {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                            )
                            .await
                            .expect("write full retry response");
                    }
                }
                _ => unreachable!(),
            }
        }
    });

    let directory = tempfile::tempdir().expect("create sequential retry output directory");
    let output = directory.path().join("retry.bin");
    let options = DownloadOptions {
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        http_auth_challenge: true,
        max_retries: 2,
        retry_wait: 0,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/retry.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(154),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
    )
    .expect("create sequential retry auth command");

    tokio::time::timeout(std::time::Duration::from_secs(8), command.execute())
        .await
        .expect("authenticated sequential retry should not hang")
        .expect("authenticated sequential retry should complete");
    assert_eq!(
        std::fs::read(&output).expect("read retried output"),
        b"0123456789"
    );
    server
        .await
        .expect("sequential retry auth fixture should finish");
}
