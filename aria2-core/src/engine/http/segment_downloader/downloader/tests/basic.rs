use super::*;

#[tokio::test]
async fn test_supports_range_no_server() {
    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(100))
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let result = dl
        .supports_range("http://127.0.0.1:1/nonexistent", None, &[])
        .await;
    assert!(result.is_err(), "should fail for unreachable host");
}

#[tokio::test]
async fn test_supports_range_rejects_error_status_with_range_header() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut request = [0u8; 2048];
        let _ = stream
            .read(&mut request)
            .await
            .expect("read should succeed");
        stream
            .write_all(
                b"HTTP/1.1 404 Not Found\r\nAccept-Ranges: bytes\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let result = dl
        .supports_range(&format!("http://{addr}"), None, &[])
        .await;

    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 404
        }))
    ));
    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("404 fixture should finish")
        .expect("404 fixture task should succeed");
}

#[tokio::test]
async fn test_download_range_zero_length() {
    ensure_rustls_provider();
    let client = reqwest::Client::new();
    let dl = HttpSegmentDownloader::new(&client);
    let result = dl
        .download_range("http://example.com", 0, 0, None, &[], None, 0)
        .await;
    assert!(result.is_ok(), "zero-length range should return empty vec");
    assert!(result.expect("already checked ok").is_empty());
}

#[tokio::test]
async fn test_downloader_creation() {
    ensure_rustls_provider();
    let client = reqwest::Client::new();
    let dl = HttpSegmentDownloader::new(&client);
    let _dl2 = HttpSegmentDownloader::new(&dl.client);
}

#[tokio::test]
async fn test_download_range_with_mock_http_416() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");

    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut buf = [0u8; 2048];
        // Use read() instead of read_exact() to avoid blocking on exact byte count
        let _n = stream.read(&mut buf).await.expect("read should succeed");
        stream.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.expect("write should succeed");
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    let url = format!("http://{}", addr);
    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);

    let result = dl
        .download_range(&url, 99999, 100, None, &[], None, 0)
        .await;
    assert!(result.is_err(), "416 should be an error");

    // Wait for server with timeout
    let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
}

#[tokio::test]
async fn test_download_range_rejects_server_that_ignores_range() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut request = [0u8; 2048];
        let _ = stream
            .read(&mut request)
            .await
            .expect("read should succeed");
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{addr}/no-range");

    let result = dl.download_range(&url, 5, 5, None, &[], None, 10).await;
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::CannotResume))
    ));

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("fixture should finish")
        .expect("fixture task should succeed");
}

#[tokio::test]
async fn test_download_range_rejects_mismatched_content_range() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept should succeed");
        let mut request = [0u8; 2048];
        let _ = stream
            .read(&mut request)
            .await
            .expect("read should succeed");
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4/10\r\nContent-Length: 5\r\nConnection: close\r\n\r\n01234",
            )
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{addr}/wrong-range");

    let result = dl.download_range(&url, 5, 5, None, &[], None, 10).await;
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::CannotResume))
    ));

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("fixture should finish")
        .expect("fixture task should succeed");
}

#[test]
fn test_classify_range_status_keeps_terminal_and_retryable_http_errors_distinct() {
    assert!(matches!(
        classify_range_status(reqwest::StatusCode::NOT_FOUND, "bytes=0-9"),
        Some(Aria2Error::Recoverable(RecoverableError::ResourceNotFound))
    ));
    assert!(matches!(
        classify_range_status(reqwest::StatusCode::SERVICE_UNAVAILABLE, "bytes=0-9"),
        Some(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 503
        }))
    ));
    assert!(matches!(
        classify_range_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "bytes=0-9"),
        Some(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 429
        }))
    ));
    assert!(matches!(
        classify_range_status(reqwest::StatusCode::REQUEST_TIMEOUT, "bytes=0-9"),
        Some(Aria2Error::Recoverable(RecoverableError::ServerError {
            code: 408
        }))
    ));
}

#[tokio::test]
async fn test_download_range_maps_not_found_to_resource_not_found() {
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
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("write should succeed");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let dl = HttpSegmentDownloader::new(&client);
    let url = format!("http://{addr}/missing");

    let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound))
    ));

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("404 fixture should finish")
        .expect("404 fixture task should succeed");
}
