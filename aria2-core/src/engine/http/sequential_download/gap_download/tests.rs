use super::*;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::engine::download_progress::ProgressUpdater;
use crate::engine::http::cookie_helper::CookieHelper;
use crate::engine::retry_policy::RetryPolicy;
use crate::http::HttpRequestPolicy;
use crate::http::cookie::CookieStorage;
use crate::request::request_group::{AtomicProgress, DownloadOptions, GroupId, RequestGroup};
use crate::util::perf_monitor::AtomicMetrics;
use crate::util::rwlock_ext::RwLockRecover;

fn make_downloader(
    uri: &str,
    output_path: std::path::PathBuf,
    group_id: u64,
) -> (SequentialDownloader, Arc<std::sync::RwLock<RequestGroup>>) {
    crate::http::client_pool::ensure_rustls_provider();
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(group_id),
        vec![uri.to_string()],
        DownloadOptions::default(),
    )));
    group.recover_mut().start().expect("start request group");
    let progress = Arc::new(AtomicProgress::new());
    let downloader = SequentialDownloader::new(
        Arc::new(
            reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build test HTTP client"),
        ),
        output_path,
        HttpRequestPolicy::default(),
        CookieHelper::new(Arc::new(CookieStorage::new()), None),
        ProgressUpdater::new(
            None,
            None,
            Arc::clone(&progress),
            Arc::new(AtomicMetrics::new()),
            None,
        ),
        Arc::clone(&group),
        progress,
        None,
    );
    (downloader, group)
}

async fn read_request_headers(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let count = stream.read(&mut chunk).await.expect("read HTTP request");
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(request).expect("HTTP request headers should be UTF-8")
}

#[test]
fn classifies_416_as_range_failure() {
    assert!(matches!(
        classify_gap_http_status(416, "bytes=10-20"),
        Aria2Error::Recoverable(RecoverableError::RangeNotSatisfiable { range })
            if range == "bytes=10-20"
    ));
}

#[test]
fn classifies_5xx_as_server_failure() {
    assert!(matches!(
        classify_gap_http_status(503, "bytes=10-20"),
        Aria2Error::Recoverable(RecoverableError::ServerError { code: 503 })
    ));
}

#[test]
fn classifies_configured_4xx_transients_as_server_failures() {
    for status_code in [408, 429] {
        assert!(matches!(
            classify_gap_http_status(status_code, "bytes=10-20"),
            Aria2Error::Recoverable(RecoverableError::ServerError { code })
                if code == status_code
        ));
    }
}

#[test]
fn classifies_other_http_statuses_as_protocol_failures() {
    assert!(matches!(
        classify_gap_http_status(404, "bytes=10-20"),
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
            if message == "HTTP error: 404"
    ));
}

#[test]
fn classifies_full_response_to_range_request_as_cannot_resume() {
    assert!(matches!(
        classify_gap_http_status(200, "bytes=10-20"),
        Aria2Error::Recoverable(RecoverableError::CannotResume)
    ));
}

#[tokio::test]
async fn gap_download_records_the_selected_peer_address() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let server_addr = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept range request");
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request).await.expect("read range request");
        stream
                .write_all(
                    b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 0-2/3\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\nabc",
                )
                .await
                .expect("write range response");
    });

    let dir = tempfile::tempdir().expect("create temporary download directory");
    let uri = format!("http://{server_addr}/payload.bin");
    let (mut downloader, group) = make_downloader(&uri, dir.path().join("payload.bin"), 9_901);

    let result = downloader.execute_with_gaps(&uri, 3, &[]).await;
    server.await.expect("test HTTP server should finish");

    assert!(
        result.error.is_none(),
        "gap download failed: {:?}",
        result.error
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("payload.bin"))
            .await
            .unwrap(),
        b"abc"
    );
    let contexts = group.recover().connection_contexts();
    assert_eq!(contexts.len(), 1);
    assert_eq!(contexts[0].endpoint.hostname(), "127.0.0.1");
    assert_eq!(contexts[0].endpoint.port(), server_addr.port());
    assert_eq!(contexts[0].peer_addr, server_addr);
}

#[tokio::test]
async fn overlapping_completed_ranges_do_not_overcount_progress() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let server_addr = listener.local_addr().expect("read listener address");
    let server = serve_scripted(
            listener,
            vec![
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 1\r\nContent-Range: bytes 5-5/6\r\nConnection: close\r\n\r\nf",
            ],
        );

    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    tokio::fs::write(&output, b"abcde\0")
        .await
        .expect("write overlapping completed ranges");
    let uri = format!("http://{server_addr}/payload.bin");
    let (mut downloader, _) = make_downloader(&uri, output.clone(), 9_906);
    let result = downloader
        .execute_with_gaps(&uri, 6, &[(0, 3), (2, 3)])
        .await;
    server.await.expect("test HTTP server should finish");

    assert!(
        result.error.is_none(),
        "gap download failed: {:?}",
        result.error
    );
    assert_eq!(result.completed_gaps, vec![(5, 1)]);
    assert_eq!(downloader.progress.completed_length(), 6);
    assert_eq!(
        tokio::fs::read(output)
            .await
            .expect("read completed output"),
        b"abcdef"
    );
}

#[tokio::test]
async fn gap_download_follows_redirect_and_preserves_the_missing_range() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let server_addr = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in [
                b"HTTP/1.1 302 Found\r\nLocation: /asset.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice(),
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\ndef".as_slice(),
            ] {
                let accepted = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    listener.accept(),
                )
                .await;
                let Ok(accepted) = accepted else {
                    break;
                };
                let (mut stream, _) = accepted.expect("accept HTTP request");
                requests.push(read_request_headers(&mut stream).await);
                stream
                    .write_all(response)
                    .await
                    .expect("write HTTP response");
            }
        requests
    });

    let dir = tempfile::tempdir().expect("create temporary download directory");
    let uri = format!("http://{server_addr}/original.bin");
    let (mut downloader, _) = make_downloader(&uri, dir.path().join("payload.bin"), 9_902);
    let result = downloader.execute_with_gaps(&uri, 6, &[(0, 3)]).await;
    let requests = server.await.expect("test HTTP server should finish");

    assert!(
        result.error.is_none(),
        "gap download failed: {:?}",
        result.error
    );
    assert_eq!(result.completed_gaps, vec![(3, 3)]);
    assert!(requests[0].starts_with("GET /original.bin HTTP/1.1\r\n"));
    assert!(requests[1].starts_with("GET /asset.bin HTTP/1.1\r\n"));
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("range: bytes=3-5\r\n")
    );
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("range: bytes=3-5\r\n")
    );
    let payload = tokio::fs::read(dir.path().join("payload.bin"))
        .await
        .expect("read downloaded gap");
    assert_eq!(&payload[3..], b"def");
}

#[tokio::test]
async fn gap_download_rejects_full_response_without_writing_it_at_the_gap_offset() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let server_addr = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept range request");
        let request = read_request_headers(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabcdef")
            .await
            .expect("write full response");
        request
    });

    let dir = tempfile::tempdir().expect("create temporary download directory");
    let uri = format!("http://{server_addr}/payload.bin");
    let (mut downloader, _) = make_downloader(&uri, dir.path().join("payload.bin"), 9_903);
    let result = downloader.execute_with_gaps(&uri, 6, &[(0, 3)]).await;
    let request = server.await.expect("test HTTP server should finish");

    assert!(
        request
            .to_ascii_lowercase()
            .contains("range: bytes=3-5\r\n")
    );
    assert!(matches!(
        result.error,
        Some(Aria2Error::Recoverable(RecoverableError::CannotResume))
    ));
    assert!(result.completed_gaps.is_empty());
    assert!(
        !tokio::fs::try_exists(dir.path().join("payload.bin"))
            .await
            .expect("check whether output was created")
    );
}

#[tokio::test]
async fn gap_download_retries_when_a_partial_response_ends_before_the_gap() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let server_addr = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in [
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 2\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\nde".as_slice(),
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 3\r\nContent-Range: bytes 3-5/6\r\nConnection: close\r\n\r\ndef".as_slice(),
            ] {
                let accepted = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    listener.accept(),
                )
                .await;
                let Ok(accepted) = accepted else {
                    break;
                };
                let (mut stream, _) = accepted.expect("accept gap request");
                requests.push(read_request_headers(&mut stream).await);
                stream
                    .write_all(response)
                    .await
                    .expect("write gap response");
            }
        requests
    });

    let dir = tempfile::tempdir().expect("create temporary download directory");
    let uri = format!("http://{server_addr}/payload.bin");
    let (mut downloader, _) = make_downloader(&uri, dir.path().join("payload.bin"), 9_904);
    let result = downloader
        .execute_with_gaps_with_retry(&uri, 6, &[(0, 3)], &RetryPolicy::new(2, 0))
        .await;
    let requests = server.await.expect("test HTTP server should finish");

    assert!(result.is_ok(), "gap retry failed: {result:?}");
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|request| {
        request
            .to_ascii_lowercase()
            .contains("range: bytes=3-5\r\n")
    }));
    let payload = tokio::fs::read(dir.path().join("payload.bin"))
        .await
        .expect("read downloaded gap");
    assert_eq!(&payload[3..], b"def");
}

/// Serve one scripted response per connection, returning the requests.
fn serve_scripted(
    listener: tokio::net::TcpListener,
    responses: Vec<&'static [u8]>,
) -> tokio::task::JoinHandle<Vec<String>> {
    tokio::spawn(async move {
        let mut requests = Vec::new();
        for response in responses {
            let accepted =
                tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept()).await;
            let Ok(accepted) = accepted else {
                break;
            };
            let (mut stream, _) = accepted.expect("accept gap request");
            requests.push(read_request_headers(&mut stream).await);
            stream
                .write_all(response)
                .await
                .expect("write gap response");
        }
        requests
    })
}

#[tokio::test]
async fn gap_finished_before_a_failed_gap_is_on_disk_after_the_retry() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind test HTTP listener");
    let server_addr = listener.local_addr().expect("read listener address");
    let server = serve_scripted(
            listener,
            vec![
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 4-7/16\r\nConnection: close\r\n\r\nEFGH",
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 12-15/16\r\nConnection: close\r\n\r\nMN",
                b"HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nContent-Range: bytes 12-15/16\r\nConnection: close\r\n\r\nMNOP",
            ],
        );

    let dir = tempfile::tempdir().expect("create temporary download directory");
    let output = dir.path().join("payload.bin");
    tokio::fs::write(&output, b"ABCD\0\0\0\0IJKL\0\0\0\0")
        .await
        .expect("write the ranges the split download finished");
    let uri = format!("http://{server_addr}/payload.bin");
    let (mut downloader, _) = make_downloader(&uri, output.clone(), 9_905);
    let result = downloader
        .execute_with_gaps_with_retry(&uri, 16, &[(0, 4), (8, 4)], &RetryPolicy::new(2, 0))
        .await;
    let requests = server.await.expect("test HTTP server should finish");

    assert!(result.is_ok(), "gap retry failed: {result:?}");
    assert_eq!(requests.len(), 3);
    assert_eq!(
        tokio::fs::read(&output).await.expect("read output"),
        b"ABCDEFGHIJKLMNOP"
    );
}
