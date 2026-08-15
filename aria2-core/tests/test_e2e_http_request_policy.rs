//! Request-level HTTP compatibility tests.
//!
//! These tests execute the production `DownloadCommand` against a real local
//! HTTP/1.1 server and assert what the server actually receives.  They cover
//! the request options implemented by aria2_original::HttpRequest rather than
//! only inspecting a reqwest builder before it is sent.

mod e2e_helpers;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use e2e_helpers::mock_http_server::{MockHttpServer, RequestLog, full_body};
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
