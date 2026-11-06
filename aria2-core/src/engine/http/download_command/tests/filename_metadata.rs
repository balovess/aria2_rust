use super::*;

#[test]
fn inferred_http_output_name_uses_the_safe_decoded_url_segment() {
    let options = DownloadOptions::default();
    let command = DownloadCommand::new(
        GroupId::new(1003),
        "https://example.com/releases/my%20file.zip?token=ignored#fragment",
        &options,
        None,
        None,
    )
    .expect("HTTP command should accept a valid URI");

    assert_eq!(
        command
            .output_path
            .file_name()
            .and_then(|name| name.to_str()),
        Some("my file.zip")
    );
}

#[tokio::test]
async fn head_content_disposition_replaces_an_inferred_http_output_name() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind filename fixture");
    let address = listener
        .local_addr()
        .expect("read filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..4 {
            let (mut stream, _) = listener.accept().await.expect("accept filename request");
            let request = read_http_request(&mut stream).await;

            if request_index == 0 {
                assert!(
                    request.starts_with("HEAD /download HTTP/1.1\r\n"),
                    "unexpected request {request_index}: {request:?}"
                );
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write HEAD redirect response");
            } else if request_index == 1 {
                assert!(
                    request.starts_with("HEAD /final HTTP/1.1\r\n"),
                    "unexpected request {request_index}: {request:?}"
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename*=UTF-8''server%20name.txt\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write final HEAD response");
            } else if request_index == 2 {
                assert!(
                    request.starts_with("GET /download HTTP/1.1\r\n"),
                    "unexpected request {request_index}: {request:?}"
                );
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write GET redirect response");
            } else {
                assert!(
                    request.starts_with("GET /final HTTP/1.1\r\n"),
                    "unexpected request {request_index}: {request:?}"
                );
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata",
                    )
                    .await
                    .expect("write GET response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create filename output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        use_head: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1004), &uri, &options, None, None)
        .expect("create inferred filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("filename download should not hang")
        .expect("filename download should complete");

    assert_eq!(
        std::fs::read(directory.path().join("server name.txt")).expect("read server filename"),
        b"data"
    );
    assert!(!directory.path().join("download").exists());
    server.await.expect("filename fixture should finish");
}

#[tokio::test]
async fn first_get_content_disposition_is_reused_for_sequential_download() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind filename fixture");
    let address = listener
        .local_addr()
        .expect("read filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);

            if request_index == 0 {
                assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write GET redirect response");
            } else {
                assert!(request.starts_with("GET /final HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ndata",
                    )
                    .await
                    .expect("write GET response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create filename output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1005), &uri, &options, None, None)
        .expect("create inferred filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("filename download should not hang")
        .expect("filename download should complete");

    assert_eq!(
        std::fs::read(directory.path().join("actual.txt")).expect("read server filename"),
        b"data"
    );
    assert!(!directory.path().join("download").exists());
    server.await.expect("filename fixture should finish");
}

#[tokio::test]
async fn inferred_content_disposition_name_enters_collision_policy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind collision filename fixture");
    let address = listener
        .local_addr()
        .expect("read collision filename fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept collision filename request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read collision filename request");
        assert!(
            String::from_utf8_lossy(&request[..bytes]).starts_with("GET /download HTTP/1.1\r\n")
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write collision filename response");
    });

    let directory = tempfile::tempdir().expect("create collision output directory");
    let existing = directory.path().join("actual.txt");
    std::fs::write(&existing, b"keep").expect("create existing output");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1006), &uri, &options, None, None)
        .expect("create collision filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("collision filename download should not hang")
        .expect("collision filename download should complete");

    assert_eq!(
        std::fs::read(&existing).expect("read existing output"),
        b"keep"
    );
    assert_eq!(
        std::fs::read(directory.path().join("actual.1.txt")).expect("read renamed output"),
        b"data"
    );
    server
        .await
        .expect("collision filename fixture should finish");
}

#[tokio::test]
async fn redirected_content_disposition_name_enters_collision_policy() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirected collision filename fixture");
    let address = listener
        .local_addr()
        .expect("read redirected collision filename fixture address");
    let server = tokio::spawn(async move {
        for request_index in 0..2 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept redirected collision filename request");
            let mut request = [0u8; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read redirected collision filename request");
            let request = String::from_utf8_lossy(&request[..bytes]);

            if request_index == 0 {
                assert!(request.starts_with("GET /download HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write redirected collision response");
            } else {
                assert!(request.starts_with("GET /final HTTP/1.1\r\n"));
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=actual.txt\r\nConnection: close\r\n\r\ndata",
                    )
                    .await
                    .expect("write redirected collision final response");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create redirected collision output directory");
    let existing = directory.path().join("actual.txt");
    std::fs::write(&existing, b"keep").expect("create redirected existing output");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command = DownloadCommand::new(GroupId::new(1009), &uri, &options, None, None)
        .expect("create redirected collision filename command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("redirected collision filename download should not hang")
        .expect("redirected collision filename download should complete");

    assert_eq!(
        std::fs::read(&existing).expect("read redirected existing output"),
        b"keep"
    );
    assert_eq!(
        std::fs::read(directory.path().join("actual.1.txt"))
            .expect("read redirected renamed output"),
        b"data"
    );
    server
        .await
        .expect("redirected collision filename fixture should finish");
}

#[tokio::test]
async fn explicit_output_name_overrides_content_disposition() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind explicit output fixture");
    let address = listener
        .local_addr()
        .expect("read explicit output fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept explicit output request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read explicit output request");
        assert!(
            String::from_utf8_lossy(&request[..bytes]).starts_with("GET /download HTTP/1.1\r\n")
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nContent-Disposition: attachment; filename=server.txt\r\nConnection: close\r\n\r\ndata",
            )
            .await
            .expect("write explicit output response");
    });

    let directory = tempfile::tempdir().expect("create explicit output directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/download");
    let mut command =
        DownloadCommand::new(GroupId::new(1011), &uri, &options, None, Some("chosen.bin"))
            .expect("create explicit output command");

    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("explicit output download should not hang")
        .expect("explicit output download should complete");

    assert_eq!(
        tokio::fs::read(directory.path().join("chosen.bin"))
            .await
            .expect("read explicit output"),
        b"data"
    );
    assert!(!directory.path().join("server.txt").exists());
    server.await.expect("explicit output fixture should finish");
}
