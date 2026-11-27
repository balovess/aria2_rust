//! Request-level HTTP compatibility tests.
//!
//! These tests execute the production `DownloadCommand` against a real local
//! HTTP/1.1 server and assert what the server actually receives.  They cover
//! the request options implemented by aria2_original::HttpRequest rather than
//! only inspecting a reqwest builder before it is sent.

mod e2e_helpers;

use aria2_core::engine::command::Command;
use aria2_core::engine::http::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use bytes::Bytes;
use e2e_helpers::mock_http_server::{
    Body, Incoming, MockHttpServer, Request, RequestLog, Response, StatusCode, full_body,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

async fn read_proxy_headers(stream: &mut tokio::net::TcpStream) -> String {
    use tokio::io::AsyncReadExt;

    let mut request = Vec::with_capacity(1024);
    let mut buffer = [0u8; 512];
    loop {
        let bytes = stream.read(&mut buffer).await.expect("read proxy request");
        assert!(bytes > 0, "proxy closed before sending request headers");
        request.extend_from_slice(&buffer[..bytes]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            return String::from_utf8(request).expect("proxy request must be HTTP text");
        }
        assert!(
            request.len() < 16 * 1024,
            "proxy request headers are too large"
        );
    }
}

fn header<'a>(request: &'a RequestLog, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

async fn download_with_options(
    server: &MockHttpServer,
    path: &str,
    options: DownloadOptions,
) -> (TempDir, Vec<RequestLog>) {
    let body: &'static [u8] = b"request-policy-e2e-body";
    server.on_get(path, move |_| {
        http::Response::builder()
            .status(http::StatusCode::OK)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", body.len())
            .body(full_body(body))
            .unwrap()
    });

    let dir = tempfile::tempdir().expect("create output directory");
    let url = format!("{}{}", server.base_url(), path);
    let mut command = DownloadCommand::new(
        GroupId::new(700),
        &url,
        &options,
        dir.path().to_str(),
        Some("output.bin"),
    )
    .expect("create download command");
    if let Err(error) = command.execute().await {
        panic!(
            "download should succeed: {error:?}; requests={:?}",
            server.take_request_log()
        );
    }

    let log = server.take_request_log();
    (dir, log)
}

#[tokio::test]
async fn default_download_is_get_without_optional_request_headers() {
    let server = MockHttpServer::start().await.expect("start server");
    let (_dir, log) = download_with_options(&server, "/default", DownloadOptions::default()).await;

    assert_eq!(
        log.len(),
        1,
        "known small file should need one GET: {log:?}"
    );
    assert_eq!(log[0].method, "GET");
    assert!(header(&log[0], "Pragma").is_none());
    assert!(header(&log[0], "Cache-Control").is_none());
    assert!(header(&log[0], "Connection").is_none());
    assert_eq!(
        header(&log[0], "Want-Digest"),
        Some("SHA-512;q=1, SHA-256;q=1, SHA;q=0.1")
    );

    server.shutdown().await;
}

#[tokio::test]
async fn request_options_reach_the_wire_and_preserve_explicit_headers() {
    let server = MockHttpServer::start().await.expect("start server");
    let options = DownloadOptions {
        http_no_cache: true,
        no_want_digest_header: true,
        enable_http_keep_alive: false,
        header: vec![
            "Cache-Control: max-age=0".to_string(),
            "X-Request-Policy: enabled".to_string(),
        ],
        ..Default::default()
    };
    let (_dir, log) = download_with_options(&server, "/options", options).await;

    assert_eq!(log.len(), 1, "expected one request: {log:?}");
    assert_eq!(log[0].method, "GET");
    assert_eq!(header(&log[0], "Pragma"), Some("no-cache"));
    assert_eq!(header(&log[0], "Cache-Control"), Some("max-age=0"));
    assert_eq!(header(&log[0], "Connection"), Some("close"));
    assert!(header(&log[0], "Want-Digest").is_none());
    assert_eq!(header(&log[0], "X-Request-Policy"), Some("enabled"));

    server.shutdown().await;
}

#[tokio::test]
async fn use_head_changes_the_first_request_method() {
    let server = MockHttpServer::start().await.expect("start server");
    let options = DownloadOptions {
        use_head: true,
        ..Default::default()
    };
    let (_dir, log) = download_with_options(&server, "/head", options).await;

    assert!(log.len() >= 2, "HEAD must be followed by the download GET");
    assert_eq!(log[0].method, "HEAD");
    assert_eq!(log[1].method, "GET");
    assert_eq!(log[0].path, "/head");
    assert_eq!(log[1].path, "/head");

    server.shutdown().await;
}

#[tokio::test]
async fn accept_gzip_is_sent_and_response_is_decoded() {
    let server = MockHttpServer::start().await.expect("start server");
    let body = b"gzip request-policy body";
    server.register_gzip_response("/gzip", body);
    let dir = tempfile::tempdir().expect("create output directory");
    let url = format!("{}/{}", server.base_url(), "gzip")
        .trim_end_matches('/')
        .to_string();
    let options = DownloadOptions {
        http_accept_gzip: true,
        ..Default::default()
    };
    let mut command = DownloadCommand::new(
        GroupId::new(701),
        &url,
        &options,
        dir.path().to_str(),
        Some("gzip.bin"),
    )
    .expect("create gzip download command");
    if let Err(error) = command.execute().await {
        panic!(
            "gzip download should succeed: {error:?}; requests={:?}",
            server.take_request_log()
        );
    }

    let output = std::fs::read(dir.path().join("gzip.bin")).expect("read gzip output");
    assert_eq!(output, body);
    let log = server.take_request_log();
    assert_eq!(log.len(), 1);
    assert_eq!(header(&log[0], "Accept-Encoding"), Some("deflate, gzip"));

    server.shutdown().await;
}

#[tokio::test]
async fn unknown_length_download_does_not_probe_with_range() {
    let server = MockHttpServer::start().await.expect("start server");
    let body = b"chunked unknown-length body";
    server.register_chunked_response(
        "/unknown-length",
        vec![b"chunked ".to_vec(), b"unknown-length body".to_vec()],
    );

    let dir = tempfile::tempdir().expect("create output directory");
    let url = format!("{}/unknown-length", server.base_url());
    let options = DownloadOptions::default();
    let mut command = DownloadCommand::new(
        GroupId::new(702),
        &url,
        &options,
        dir.path().to_str(),
        Some("unknown-length.bin"),
    )
    .expect("create download command");
    command
        .execute()
        .await
        .expect("unknown-length download should succeed");

    assert_eq!(
        std::fs::read(dir.path().join("unknown-length.bin")).expect("read output"),
        body
    );

    let log = server.take_request_log();
    assert_eq!(log.len(), 1, "unknown-length download should issue one GET");
    assert_eq!(log[0].method, "GET");
    assert!(header(&log[0], "Range").is_none());

    server.shutdown().await;
}

#[tokio::test]
async fn explicit_split_unknown_length_starts_with_one_ordinary_get() {
    let server = MockHttpServer::start().await.expect("start server");
    let body = vec![b's'; 2 * 1024 * 1024 + 17];
    server.register_chunked_response("/split-unknown", vec![body.clone()]);

    let dir = tempfile::tempdir().expect("create output directory");
    let url = format!("{}/split-unknown", server.base_url());
    let options = DownloadOptions {
        split: Some(4),
        ..Default::default()
    };
    let mut command = DownloadCommand::new(
        GroupId::new(703),
        &url,
        &options,
        dir.path().to_str(),
        Some("split-unknown.bin"),
    )
    .expect("create download command");
    command
        .execute()
        .await
        .expect("explicit split download should succeed");

    assert_eq!(
        std::fs::read(dir.path().join("split-unknown.bin")).expect("read output"),
        body
    );

    let log = server.take_request_log();
    assert_eq!(
        log.len(),
        1,
        "unknown length must not be discovered with a synthetic request: {log:?}"
    );
    assert_eq!(log[0].method, "GET");
    assert!(header(&log[0], "Range").is_none());

    server.shutdown().await;
}

#[tokio::test]
async fn configured_http_proxy_auth_downloads_through_real_proxy() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy fixture");
    let proxy_addr = listener.local_addr().expect("read proxy address");
    let body = b"proxy-authenticated-download".to_vec();
    let expected_body = body.clone();
    let proxy = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept proxy request");
        let request = read_proxy_headers(&mut stream).await;
        assert!(
            request.starts_with("GET http://origin.example/proxy-auth.bin HTTP/1.1\r\n"),
            "HTTP proxies receive the absolute target URI: {request}"
        );
        let proxy_auth = request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
            .map(|(_, value)| value.trim());
        assert_eq!(
            proxy_auth,
            Some("Basic dXNlcjpwYXNz"),
            "configured proxy credentials must reach the proxy: {request}"
        );

        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("write proxy response headers");
        stream
            .write_all(&body)
            .await
            .expect("write proxy response body");
    });

    let directory = tempfile::tempdir().expect("create output directory");
    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        http_proxy_user: Some("user".to_string()),
        http_proxy_passwd: Some("pass".to_string()),
        ..DownloadOptions::default()
    };
    let url = "http://origin.example/proxy-auth.bin";
    let mut command = DownloadCommand::new(
        GroupId::new(704),
        url,
        &options,
        directory.path().to_str(),
        Some("proxy-auth.bin"),
    )
    .expect("create proxied download command");
    command
        .execute()
        .await
        .expect("download through authenticated proxy should succeed");

    assert_eq!(
        std::fs::read(directory.path().join("proxy-auth.bin")).expect("read downloaded file"),
        expected_body
    );
    proxy.await.expect("proxy fixture should finish");
}

#[tokio::test]
async fn expiring_redirect_target_is_refreshed_for_later_range_requests() {
    const TOTAL: u64 = 2 * 1024 * 1024;
    let payload: Vec<u8> = (0..TOTAL as usize)
        .map(|index| (index % 251) as u8)
        .collect();
    let body = Bytes::from(payload.clone());
    let server = MockHttpServer::start().await.expect("start server");

    let generation = Arc::new(AtomicUsize::new(0));
    let min_valid_generation = Arc::new(AtomicUsize::new(1));
    let signed_hits = Arc::new(AtomicUsize::new(0));

    let stable_generation = Arc::clone(&generation);
    server.on_get("/stable", move |_req: &Request<Incoming>| {
        let next = stable_generation.fetch_add(1, Ordering::SeqCst) + 1;
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", format!("/signed?gen={next}"))
            .body(full_body("redirect"))
            .unwrap()
    });

    let signed_body = body.clone();
    let signed_min_generation = Arc::clone(&min_valid_generation);
    let signed_hits_for_handler = Arc::clone(&signed_hits);
    server.on_get("/signed", move |req: &Request<Incoming>| {
        let requested_generation = req
            .uri()
            .query()
            .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("gen=")))
            .and_then(|value| value.parse::<usize>().ok());
        let Some(token_generation) = requested_generation else {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(full_body("missing generation"))
                .unwrap();
        };
        if token_generation < signed_min_generation.load(Ordering::SeqCst) {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(full_body("expired generation"))
                .unwrap();
        }

        // The metadata HEAD and Range probe both use generation 1. Expire it
        // immediately afterwards: only a request that re-enters /stable can
        // obtain a generation the payload endpoint still accepts.
        let hit = signed_hits_for_handler.fetch_add(1, Ordering::SeqCst) + 1;
        if hit == 2 {
            signed_min_generation.store(2, Ordering::SeqCst);
        }

        let Some(range) = header_from_request(req, "Range") else {
            return Response::builder()
                .status(StatusCode::OK)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", TOTAL)
                .body(full_body(signed_body.clone()))
                .unwrap();
        };
        let Some((start, end)) = parse_byte_range(range) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{TOTAL}"))
                .body(full_body("bad range"))
                .unwrap();
        };
        if start >= TOTAL || end >= TOTAL || end < start {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{TOTAL}"))
                .body(full_body("range outside payload"))
                .unwrap();
        }
        let chunk = signed_body.slice(start as usize..=end as usize);
        response_for_range(chunk, start, end, TOTAL)
    });

    let options = DownloadOptions {
        use_head: true,
        split: Some(2),
        max_connection_per_server: Some(2),
        min_split_size: Some(1),
        max_retries: 0,
        retry_wait: 0,
        ..Default::default()
    };
    let url = format!("{}/stable", server.base_url());
    let directory = tempfile::tempdir().expect("create output directory");
    let mut command = DownloadCommand::new(
        GroupId::new(706),
        &url,
        &options,
        directory.path().to_str(),
        Some("expiring-redirect.bin"),
    )
    .expect("create expiring redirect download command");

    if let Err(error) = command.execute().await {
        panic!(
            "download should refresh expired redirect targets: {error:?}; requests={:?}",
            server.take_request_log()
        );
    }

    let downloaded = std::fs::read(directory.path().join("expiring-redirect.bin"))
        .expect("read downloaded file");
    assert_eq!(downloaded, payload);

    let log = server.take_request_log();
    let stable_hits = generation.load(Ordering::SeqCst);
    assert!(
        stable_hits >= 3,
        "each later Range request should re-enter the stable URL; stable hits: {stable_hits}; requests={log:?}"
    );
    assert!(
        log.iter()
            .filter(|request| request.path == "/stable")
            .count()
            >= 3,
        "server should observe fresh redirects after the signed URL expired: {log:?}"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn valid_redirect_target_is_reused_by_later_range_requests() {
    const TOTAL: u64 = 2 * 1024 * 1024;
    let payload: Vec<u8> = (0..TOTAL as usize)
        .map(|index| (index % 251) as u8)
        .collect();
    let body = Bytes::from(payload.clone());
    let server = MockHttpServer::start().await.expect("start server");
    let stable_hits = Arc::new(AtomicUsize::new(0));

    let stable_counter = Arc::clone(&stable_hits);
    server.on_get("/stable", move |_req: &Request<Incoming>| {
        let generation = stable_counter.fetch_add(1, Ordering::SeqCst) + 1;
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", format!("/asset?gen={generation}"))
            .body(full_body("redirect"))
            .unwrap()
    });

    let asset_body = body.clone();
    server.on_get("/asset", move |req: &Request<Incoming>| {
        let Some(range) = header_from_request(req, "Range") else {
            return Response::builder()
                .status(StatusCode::OK)
                .header("Accept-Ranges", "bytes")
                .header("Content-Length", TOTAL)
                .body(full_body(asset_body.clone()))
                .unwrap();
        };
        let Some((start, end)) = parse_byte_range(range) else {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{TOTAL}"))
                .body(full_body("bad range"))
                .unwrap();
        };
        if start >= TOTAL || end >= TOTAL || end < start {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{TOTAL}"))
                .body(full_body("range outside payload"))
                .unwrap();
        }
        let chunk = asset_body.slice(start as usize..=end as usize);
        response_for_range(chunk, start, end, TOTAL)
    });

    let options = DownloadOptions {
        use_head: true,
        split: Some(2),
        max_connection_per_server: Some(2),
        min_split_size: Some(1),
        max_retries: 0,
        retry_wait: 0,
        ..Default::default()
    };
    let url = format!("{}/stable", server.base_url());
    let directory = tempfile::tempdir().expect("create output directory");
    let mut command = DownloadCommand::new(
        GroupId::new(707),
        &url,
        &options,
        directory.path().to_str(),
        Some("valid-redirect.bin"),
    )
    .expect("create valid redirect download command");

    command
        .execute()
        .await
        .expect("download with valid redirect should succeed");

    let downloaded =
        std::fs::read(directory.path().join("valid-redirect.bin")).expect("read downloaded file");
    assert_eq!(downloaded, payload);
    assert_eq!(
        stable_hits.load(Ordering::SeqCst),
        1,
        "valid final URL should be reused without re-entering the stable URL for every Range"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn primary_redirect_refresh_with_secondary_mirror_configured() {
    const TOTAL: u64 = 4 * 1024 * 1024;
    let payload: Vec<u8> = (0..TOTAL as usize)
        .map(|index| (index % 251) as u8)
        .collect();
    let body = Bytes::from(payload.clone());
    let server = MockHttpServer::start().await.expect("start server");

    let stable_a_hits = Arc::new(AtomicUsize::new(0));
    let stable_b_hits = Arc::new(AtomicUsize::new(0));
    let min_valid_a = Arc::new(AtomicUsize::new(1));
    let signed_a_hits = Arc::new(AtomicUsize::new(0));
    let forbidden_a_hits = Arc::new(AtomicUsize::new(0));

    let counter_a = Arc::clone(&stable_a_hits);
    server.on_get("/stable-a", move |_req: &Request<Incoming>| {
        let generation = counter_a.fetch_add(1, Ordering::SeqCst) + 1;
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", format!("/cdn/a?gen={generation}"))
            .body(full_body("redirect"))
            .unwrap()
    });

    let counter_b = Arc::clone(&stable_b_hits);
    server.on_get("/stable-b", move |_req: &Request<Incoming>| {
        let generation = counter_b.fetch_add(1, Ordering::SeqCst) + 1;
        Response::builder()
            .status(StatusCode::FOUND)
            .header("Location", format!("/cdn/b?gen={generation}"))
            .body(full_body("redirect"))
            .unwrap()
    });

    let a_min = Arc::clone(&min_valid_a);
    let a_hits = Arc::clone(&signed_a_hits);
    let a_forbidden = Arc::clone(&forbidden_a_hits);
    let a_body = body.clone();
    server.on_get("/cdn/a", move |req: &Request<Incoming>| {
        serve_signed_range(req, &a_body, TOTAL, Some((&a_min, &a_hits, &a_forbidden)))
    });

    let b_body = body.clone();
    server.on_get("/cdn/b", move |req: &Request<Incoming>| {
        serve_signed_range(req, &b_body, TOTAL, None)
    });

    let options = DownloadOptions {
        use_head: true,
        split: Some(4),
        max_connection_per_server: Some(4),
        min_split_size: Some(1),
        uri_selector: "inorder".to_string(),
        max_retries: 0,
        retry_wait: 0,
        ..Default::default()
    };
    let url_a = format!("{}/stable-a", server.base_url());
    let url_b = format!("{}/stable-b", server.base_url());
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(708),
        vec![url_a.clone(), url_b.clone()],
        options.clone(),
    )));
    let directory = tempfile::tempdir().expect("create output directory");
    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url_a,
        &options,
        directory.path().to_str(),
        Some("multi-mirror-redirect.bin"),
    )
    .expect("create multi-mirror redirect command");

    command.execute().await.unwrap_or_else(|error| {
        panic!(
            "multi-mirror download should refresh expired redirects: {error:?}; requests={:?}",
            server.take_request_log()
        )
    });

    let downloaded = std::fs::read(directory.path().join("multi-mirror-redirect.bin"))
        .expect("read downloaded file");
    assert_eq!(downloaded, payload);
    assert!(
        stable_a_hits.load(Ordering::SeqCst) >= 2,
        "primary source should be re-entered after its signed URL expired"
    );
    assert!(
        forbidden_a_hits.load(Ordering::SeqCst) > 0,
        "primary signed URL should be observed as expired before refresh"
    );

    server.shutdown().await;
}

fn serve_signed_range(
    req: &Request<Incoming>,
    body: &Bytes,
    total: u64,
    expiry: Option<(&Arc<AtomicUsize>, &Arc<AtomicUsize>, &Arc<AtomicUsize>)>,
) -> Response<Body> {
    if let Some((min_valid, hits, forbidden)) = expiry {
        let Some(generation) = req
            .uri()
            .query()
            .and_then(|query| query.split('&').find_map(|pair| pair.strip_prefix("gen=")))
            .and_then(|value| value.parse::<usize>().ok())
        else {
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(full_body("missing generation"))
                .unwrap();
        };
        if generation < min_valid.load(Ordering::SeqCst) {
            forbidden.fetch_add(1, Ordering::SeqCst);
            return Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(full_body("expired generation"))
                .unwrap();
        }
        let hit = hits.fetch_add(1, Ordering::SeqCst) + 1;
        if hit == 2 {
            min_valid.store(2, Ordering::SeqCst);
        }
    }

    let Some(range) = header_from_request(req, "Range") else {
        return Response::builder()
            .status(StatusCode::OK)
            .header("Accept-Ranges", "bytes")
            .header("Content-Length", total)
            .body(full_body(body.clone()))
            .unwrap();
    };
    let Some((start, end)) = parse_byte_range(range) else {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header("Content-Range", format!("bytes */{total}"))
            .body(full_body("bad range"))
            .unwrap();
    };
    if start >= total || end >= total || end < start {
        return Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header("Content-Range", format!("bytes */{total}"))
            .body(full_body("range outside payload"))
            .unwrap();
    }
    response_for_range(body.slice(start as usize..=end as usize), start, end, total)
}

fn header_from_request<'a>(request: &'a Request<Incoming>, name: &str) -> Option<&'a str> {
    request
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn parse_byte_range(value: &str) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    let (start, end) = value.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

fn response_for_range(chunk: Bytes, start: u64, end: u64, total: u64) -> Response<Body> {
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header("Accept-Ranges", "bytes")
        .header("Content-Range", format!("bytes {start}-{end}/{total}"))
        .header("Content-Length", chunk.len())
        .body(full_body(chunk))
        .unwrap()
}

#[tokio::test]
async fn embedded_proxy_url_credentials_survive_407_fallback() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy fixture");
    let proxy_addr = listener.local_addr().expect("read proxy address");
    let body = b"proxy-url-fallback".to_vec();
    let expected_body = body.clone();
    let proxy = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept proxy request");
            let request = read_proxy_headers(&mut stream).await;
            assert!(
                request
                    .starts_with("GET http://origin.example/proxy-url-fallback.bin HTTP/1.1\r\n"),
                "HTTP proxy must receive the absolute target URI: {request}"
            );

            let proxy_auth = request
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
                .map(|(_, value)| value.trim());

            if attempt == 0 {
                // The transport may already send URL userinfo preemptively.
                // Force the production 407 path so the retry resolver is tested.
                let response = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                stream
                    .write_all(response)
                    .await
                    .expect("write proxy challenge");
            } else {
                assert_eq!(
                    proxy_auth,
                    Some("Basic dXJsLXVzZXI6dXJsLXBhc3M="),
                    "embedded proxy URL credentials must be available to 407 retry: {request}"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write proxy response headers");
                stream
                    .write_all(&body)
                    .await
                    .expect("write proxy response body");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create output directory");
    let options = DownloadOptions {
        http_proxy: Some(format!("http://url-user:url-pass@{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let url = "http://origin.example/proxy-url-fallback.bin";
    let mut command = DownloadCommand::new(
        GroupId::new(705),
        url,
        &options,
        directory.path().to_str(),
        Some("proxy-url-fallback.bin"),
    )
    .expect("create proxy URL credential download command");
    command
        .execute()
        .await
        .expect("download should succeed after proxy 407 fallback");

    assert_eq!(
        std::fs::read(directory.path().join("proxy-url-fallback.bin"))
            .expect("read downloaded file"),
        expected_body
    );
    proxy.await.expect("proxy fixture should finish");
}
