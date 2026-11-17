use super::*;

#[tokio::test]
async fn cross_origin_redirect_drops_origin_credentials_and_sensitive_custom_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let target_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect target fixture");
    let target_address = target_listener
        .local_addr()
        .expect("read redirect target address");
    let target = tokio::spawn(async move {
        let (mut stream, _) = target_listener
            .accept()
            .await
            .expect("accept redirected request");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read redirected request");
        let request = String::from_utf8_lossy(&request[..bytes]).to_ascii_lowercase();
        assert!(request.starts_with("get /target.bin http/1.1\r\n"));
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("x-api-key:"));
        assert!(!request.contains("x-session-token:"));
        assert!(request.contains("x-trace-id: trace-123\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n")
            .await
            .expect("write redirect target response");
    });

    let source_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect source fixture");
    let source_address = source_listener
        .local_addr()
        .expect("read redirect source address");
    let source = tokio::spawn(async move {
        for request_number in 1..=2 {
            let (mut stream, _) = source_listener
                .accept()
                .await
                .expect("accept source request");
            let mut request = vec![0; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read source request");
            let request = String::from_utf8_lossy(&request[..bytes]).to_ascii_lowercase();
            assert!(request.contains("x-api-key: api-secret\r\n"));
            assert!(request.contains("x-session-token: session-secret\r\n"));
            if request_number == 1 {
                assert!(!request.contains("authorization:"));
                stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"origin\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write origin auth challenge");
            } else {
                assert!(
                    request.contains("authorization: basic dxnlcjpwyxnzd29yza==\r\n"),
                    "source auth retry should include configured credentials: {request}"
                );
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/target.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write cross-origin redirect");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create redirect output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_user: Some("user".to_string()),
        http_passwd: Some("password".to_string()),
        http_auth_challenge: true,
        header: vec![
            "X-API-Key: api-secret".to_string(),
            "X-Session-Token: session-secret".to_string(),
            "X-Trace-ID: trace-123".to_string(),
        ],
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{source_address}/source.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(151),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("redirect.bin"),
    )
    .expect("create cross-origin redirect command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("cross-origin redirect download should not hang")
        .expect("cross-origin redirect download should complete");
    assert_eq!(
        std::fs::read(directory.path().join("redirect.bin")).expect("read redirect output"),
        b"ok\n"
    );
    source.await.expect("redirect source fixture should finish");
    target.await.expect("redirect target fixture should finish");
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
