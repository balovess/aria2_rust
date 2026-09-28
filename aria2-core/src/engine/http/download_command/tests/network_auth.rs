use super::*;

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
async fn proxied_client_binds_source_for_the_proxy_peer() {
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind source-aware proxy fixture");
    let proxy_addr = listener
        .local_addr()
        .expect("read source-aware proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener
            .accept()
            .await
            .expect("accept source-aware proxy request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read source-aware proxy request");
        assert!(bytes > 0, "proxy request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write source-aware proxy response");
        peer
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1201),
        vec!["http://[::1]:9/file.bin".to_string()],
        options.clone(),
    )));
    let policy = Arc::new(
        OutboundNetworkPolicy::new(vec![
            IpAddr::V6("::1".parse().expect("parse IPv6 source")),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
        ])
        .expect("source policy should accept both address families"),
    );
    let command = DownloadCommand::new_with_group_and_resolved_addresses_and_policy(
        group,
        "http://[::1]:9/file.bin",
        &options,
        None,
        None,
        None,
        policy,
    )
    .expect("proxy client must select a source compatible with the proxy peer");

    let response = command
        .client
        .get("http://[::1]:9/file.bin")
        .send()
        .await
        .expect("proxy should serve the IPv6-target request");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        server.await.expect("proxy fixture should finish").ip(),
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))
    );
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
        use_head: false,
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
        use_head: false,
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
