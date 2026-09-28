use super::*;

#[tokio::test]
async fn test_download_range_streaming_maps_ordinary_4xx_to_http_protocol_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut request = [0u8; 2048];
        let _n = stream
            .read(&mut request)
            .await
            .expect("read should succeed");
        stream
            .write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{addr}/forbidden");
    let (write_tx, _write_rx) = mpsc::channel(8);

    let result = dl
        .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
        .await;
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(
            RecoverableError::HttpProtocolError { message }
        )) if message.contains("403")
    ));

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("403 fixture should finish")
        .expect("403 fixture task should succeed");
}

#[tokio::test]
async fn test_supports_range_header_parsing() {
    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);

    match dl
        .supports_range(
            "http://invalid-host-name-that-does-not-exist-12345.com/",
            None,
            &[],
        )
        .await
    {
        Ok(supports) => {
            eprintln!(
                "[WARN] Unexpected success for invalid host, supports={:?}",
                supports
            );
        }
        Err(e) => {
            println!("Expected network error for invalid host: {:?}", e);
        }
    }
}

#[tokio::test]
async fn test_download_range_streaming_short_body_is_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await.expect("read should succeed");
        stream
            .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234")
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{}", addr);
    let (write_tx, _write_rx) = mpsc::channel(8);

    let result = dl
        .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
        .await;
    assert!(result.is_err(), "short streaming body should be rejected");
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

#[tokio::test]
async fn test_download_range_short_body_is_rejected() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await.expect("read should succeed");
        stream
            .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234")
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{}", addr);

    let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
    assert!(result.is_err(), "short 206 body should be rejected");
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

#[tokio::test]
async fn test_download_range_status_code_handling() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut buf = [0u8; 2048];
        let _ = stream.read(&mut buf).await.expect("read should succeed");
        stream
            .write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789")
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{}", addr);

    let result = dl
        .download_range(&url, 0, 10, None, &[], None, 20)
        .await
        .expect("matching 206 should succeed");
    assert_eq!(result.as_ref(), b"0123456789");
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

#[tokio::test]
async fn test_download_range_follows_redirect_before_validating_range() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        for response in [
            b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789"
                .as_slice(),
        ] {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0u8; 2048];
            let bytes = stream.read(&mut request).await.expect("read should succeed");
            assert!(bytes > 0, "request should not be empty");
            stream.write_all(response).await.expect("write should succeed");
        }
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{addr}/source");

    let result = dl
        .download_range(&url, 0, 10, None, &[], None, 20)
        .await
        .expect("range download should follow redirect");
    assert_eq!(result.as_ref(), b"0123456789");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("redirect fixture should finish")
        .expect("redirect fixture task should succeed");
}

#[tokio::test]
async fn test_streaming_range_follows_redirect_before_validating_range() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        for response in [
            b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .as_slice(),
            b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789"
                .as_slice(),
        ] {
            let (mut stream, _) = listener.accept().await.expect("accept should succeed");
            let mut request = [0u8; 2048];
            let bytes = stream.read(&mut request).await.expect("read should succeed");
            assert!(bytes > 0, "request should not be empty");
            stream.write_all(response).await.expect("write should succeed");
        }
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{addr}/source");
    let (write_tx, mut write_rx) = mpsc::channel(8);

    let total = dl
        .download_range_streaming(&url, 0, 10, None, &[], None, &write_tx, 20)
        .await
        .expect("streaming range download should follow redirect");
    drop(write_tx);

    let mut output = Vec::new();
    while let Some(chunk) = write_rx.recv().await {
        output.extend_from_slice(&chunk.data);
    }
    assert_eq!(total, 10);
    assert_eq!(output, b"0123456789");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("redirect fixture should finish")
        .expect("redirect fixture task should succeed");
}

#[tokio::test]
async fn test_range_redirect_propagates_set_cookie_to_next_request() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut redirect_stream, _) = listener.accept().await.expect("accept redirect");
        let mut redirect_request = [0u8; 2048];
        let bytes = redirect_stream
            .read(&mut redirect_request)
            .await
            .expect("read redirect request");
        assert!(bytes > 0, "redirect request should not be empty");
        redirect_stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /target\r\nSet-Cookie: sid=abc; Path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write redirect response");

        let (mut target_stream, _) = listener.accept().await.expect("accept target");
        let mut target_request = [0u8; 2048];
        let bytes = target_stream
            .read(&mut target_request)
            .await
            .expect("read target request");
        let target_request = String::from_utf8_lossy(&target_request[..bytes]);
        assert!(
            target_request
                .lines()
                .any(|line| line.eq_ignore_ascii_case("Cookie: sid=abc")),
            "redirect target must receive the cookie set by the redirect response: {target_request}"
        );
        target_stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write target response");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let cookie_helper = CookieHelper::new(
        std::sync::Arc::new(crate::http::cookie::CookieStorage::new()),
        None,
    );
    let dl = HttpSegmentDownloader::new(&client).with_cookie_helper(cookie_helper);
    let url = format!("http://{addr}/source");

    let result = dl
        .download_range(&url, 0, 10, None, &[], None, 20)
        .await
        .expect("range download should retain redirect cookies");
    assert_eq!(result.as_ref(), b"0123456789");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("cookie redirect fixture should finish")
        .expect("cookie redirect fixture task should succeed");
}
