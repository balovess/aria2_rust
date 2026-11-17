use super::*;

#[tokio::test]
async fn test_range_probe_shares_challenge_auth_with_next_range_downloader() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut first_stream, _) = listener.accept().await.expect("accept first request");
        let mut first_request = [0u8; 4096];
        let bytes = first_stream
            .read(&mut first_request)
            .await
            .expect("read first request");
        let first_request = String::from_utf8_lossy(&first_request[..bytes]);
        assert!(first_request.starts_with("GET /file.bin HTTP/1.1"));
        assert!(has_header(&first_request, "Range", "bytes=0-0"));
        assert!(!has_header_name(&first_request, "Authorization"));
        first_stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write auth challenge");

        let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
        let mut second_request = [0u8; 4096];
        let bytes = second_stream
            .read(&mut second_request)
            .await
            .expect("read retry request");
        let second_request = String::from_utf8_lossy(&second_request[..bytes]);
        assert!(has_header(&second_request, "Range", "bytes=0-0"));
        assert!(has_header(
            &second_request,
            "Authorization",
            "Basic dXNlcjpwYXNz"
        ));
        second_stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/20\r\nContent-Length: 1\r\nConnection: close\r\n\r\n0",
            )
            .await
            .expect("write authenticated response");

        let (mut third_stream, _) = listener.accept().await.expect("accept next range");
        let mut third_request = [0u8; 4096];
        let bytes = third_stream
            .read(&mut third_request)
            .await
            .expect("read next range");
        let third_request = String::from_utf8_lossy(&third_request[..bytes]);
        assert!(has_header(&third_request, "Range", "bytes=10-19"));
        assert!(has_header(
            &third_request,
            "Authorization",
            "Basic dXNlcjpwYXNz"
        ));
        third_stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 10-19/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabcdefghij",
            )
            .await
            .expect("write preemptively authenticated response");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let auth_options = AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..AuthResolveOptions::default()
    };
    let auth_factory = std::sync::Arc::new(std::sync::Mutex::new(
        crate::http::auth::AuthConfigFactory::new(),
    ));
    let dl = HttpSegmentDownloader::new(&client)
        .with_shared_auth_options(auth_options.clone(), std::sync::Arc::clone(&auth_factory));
    let url = format!("http://{addr}/file.bin");

    let probe = dl
        .probe_range_metadata(&url, None)
        .await
        .expect("authenticated range probe should succeed");
    assert!(probe.supports_range);

    let next_downloader =
        HttpSegmentDownloader::new(&client).with_shared_auth_options(auth_options, auth_factory);
    let next_result = next_downloader
        .download_range(&url, 10, 10, None, &[], None, 20)
        .await
        .expect("shared credentials should authenticate the next range preemptively");
    assert_eq!(next_result.as_ref(), b"abcdefghij");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("auth fixture should finish")
        .expect("auth fixture task should succeed");
}

#[tokio::test]
async fn test_range_auth_retry_follows_cross_origin_redirect_without_origin_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect target");
    let target_addr = target_listener.local_addr().expect("target address");
    let target = tokio::spawn(async move {
        let (mut stream, _) = target_listener
            .accept()
            .await
            .expect("accept target request");
        let mut request = [0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read target request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /payload.bin HTTP/1.1"));
        assert!(has_header(&request, "Range", "bytes=0-0"));
        assert!(!has_header_name(&request, "Authorization"));
        assert!(!has_header_name(&request, "X-API-Key"));
        assert!(has_header(&request, "X-Trace-ID", "trace-123"));
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/10\r\nContent-Length: 1\r\nConnection: close\r\n\r\na",
            )
            .await
            .expect("write target range response");
    });

    let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect source");
    let source_addr = source_listener.local_addr().expect("source address");
    let source = tokio::spawn(async move {
        let (mut challenge_stream, _) = source_listener
            .accept()
            .await
            .expect("accept initial range request");
        let mut request = [0; 4096];
        let bytes = challenge_stream
            .read(&mut request)
            .await
            .expect("read initial range request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(!has_header_name(&request, "Authorization"));
        assert!(has_header(&request, "X-API-Key", "api-secret"));
        challenge_stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write auth challenge");

        let (mut auth_stream, _) = source_listener
            .accept()
            .await
            .expect("accept authenticated retry");
        let mut request = [0; 4096];
        let bytes = auth_stream
            .read(&mut request)
            .await
            .expect("read authenticated retry");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(has_header(&request, "Authorization", "Basic dXNlcjpwYXNz"));
        assert!(has_header(&request, "X-API-Key", "api-secret"));
        auth_stream
            .write_all(
                format!(
                    "HTTP/1.1 302 Found\r\nLocation: http://{target_addr}/payload.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .expect("write cross-origin redirect");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let policy = crate::http::HttpRequestPolicy::new(
        vec![
            ("X-API-Key".to_string(), "api-secret".to_string()),
            ("X-Trace-ID".to_string(), "trace-123".to_string()),
        ],
        false,
        false,
        false,
        true,
        false,
    );
    let auth_options = AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..AuthResolveOptions::default()
    };
    let downloader = HttpSegmentDownloader::new_with_policy(&client, policy)
        .with_auth_options(auth_options, None);
    let url = format!("http://{source_addr}/protected.bin");
    let probe = tokio::time::timeout(
        Duration::from_secs(5),
        downloader.probe_range_metadata(&url, None),
    )
    .await
    .expect("range probe should not hang")
    .expect("authenticated redirected range probe should succeed");

    assert!(probe.supports_range);
    assert_eq!(
        probe.effective_url,
        format!("http://{target_addr}/payload.bin")
    );
    assert!(!probe.origin_sensitive_headers_allowed);
    assert_eq!(probe.total_length, 10);
    tokio::time::timeout(Duration::from_secs(2), source)
        .await
        .expect("source fixture should finish")
        .expect("source fixture task should succeed");
    tokio::time::timeout(Duration::from_secs(2), target)
        .await
        .expect("target fixture should finish")
        .expect("target fixture task should succeed");
}

#[tokio::test]
async fn test_range_sends_preemptive_basic_auth_credentials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept request");
        let mut request = [0u8; 4096];
        let bytes = stream.read(&mut request).await.expect("read request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(has_header(&request, "Range", "bytes=0-9"));
        assert!(has_header(&request, "Authorization", "Basic dXNlcjpwYXNz"));
        stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated response");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let auth_options = AuthResolveOptions {
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..AuthResolveOptions::default()
    };
    let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
    let url = format!("http://{addr}/file.bin");

    let result = dl
        .download_range(&url, 0, 10, None, &[], None, 20)
        .await
        .expect("preemptively authenticated range download should succeed");
    assert_eq!(result.as_ref(), b"0123456789");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("preemptive auth fixture should finish")
        .expect("preemptive auth fixture task should succeed");
}

#[tokio::test]
async fn test_range_verifies_digest_auth_response() {
    use md5::{Digest, Md5};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn md5_hex(value: &str) -> String {
        let mut hasher = Md5::new();
        hasher.update(value.as_bytes());
        hex::encode(hasher.finalize())
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut first_stream, _) = listener.accept().await.expect("accept first request");
        let mut first_request = [0u8; 4096];
        let bytes = first_stream
            .read(&mut first_request)
            .await
            .expect("read first request");
        let first_request = String::from_utf8_lossy(&first_request[..bytes]);
        assert!(!has_header_name(&first_request, "Authorization"));
        first_stream
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"download\", nonce=\"fixed-nonce\", qop=\"auth\", algorithm=MD5, opaque=\"opaque\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write digest challenge");

        let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
        let mut second_request = [0u8; 4096];
        let bytes = second_stream
            .read(&mut second_request)
            .await
            .expect("read retry request");
        let second_request = String::from_utf8_lossy(&second_request[..bytes]);
        let authorization =
            header_value(&second_request, "Authorization").expect("digest auth header");
        assert!(authorization.starts_with("Digest "));
        assert_eq!(digest_parameter(authorization, "username"), Some("user"));
        assert_eq!(digest_parameter(authorization, "realm"), Some("download"));
        assert_eq!(
            digest_parameter(authorization, "nonce"),
            Some("fixed-nonce")
        );
        assert_eq!(digest_parameter(authorization, "uri"), Some("/file.bin"));
        assert_eq!(digest_parameter(authorization, "qop"), Some("auth"));
        assert_eq!(digest_parameter(authorization, "nc"), Some("00000001"));
        assert_eq!(digest_parameter(authorization, "opaque"), Some("opaque"));

        let cnonce = digest_parameter(authorization, "cnonce").expect("digest cnonce");
        let response = digest_parameter(authorization, "response").expect("digest response");
        let ha1 = md5_hex("user:download:pass");
        let ha2 = md5_hex("GET:/file.bin");
        let expected = md5_hex(&format!("{ha1}:fixed-nonce:00000001:{cnonce}:auth:{ha2}"));
        assert_eq!(response, expected);

        second_stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated response");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let auth_options = AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..AuthResolveOptions::default()
    };
    let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
    let url = format!("http://{addr}/file.bin");

    let result = dl
        .download_range(&url, 0, 10, None, &[], None, 20)
        .await
        .expect("digest-authenticated range download should succeed");
    assert_eq!(result.as_ref(), b"0123456789");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("digest auth fixture should finish")
        .expect("digest auth fixture task should succeed");
}

#[tokio::test]
async fn test_range_retries_proxy_auth_challenge() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        let (mut first_stream, _) = listener.accept().await.expect("accept first request");
        let mut first_request = [0u8; 4096];
        let bytes = first_stream
            .read(&mut first_request)
            .await
            .expect("read first request");
        let first_request = String::from_utf8_lossy(&first_request[..bytes]);
        assert!(!has_header_name(&first_request, "Proxy-Authorization"));
        first_stream
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write proxy auth challenge");

        let (mut second_stream, _) = listener.accept().await.expect("accept retry request");
        let mut second_request = [0u8; 4096];
        let bytes = second_stream
            .read(&mut second_request)
            .await
            .expect("read retry request");
        let second_request = String::from_utf8_lossy(&second_request[..bytes]);
        assert!(has_header(
            &second_request,
            "Proxy-Authorization",
            "Basic dXNlcjpwYXNz"
        ));
        assert!(has_header(&second_request, "Range", "bytes=0-9"));
        second_stream
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-9/20\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated response");
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let auth_options = AuthResolveOptions {
        proxy_user: Some("user".to_string()),
        proxy_passwd: Some("pass".to_string()),
        ..AuthResolveOptions::default()
    };
    let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
    let url = format!("http://{addr}/file.bin");

    let result = dl
        .download_range(&url, 0, 10, None, &[], None, 20)
        .await
        .expect("proxy-authenticated range download should succeed");
    assert_eq!(result.as_ref(), b"0123456789");

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("proxy auth fixture should finish")
        .expect("proxy auth fixture task should succeed");
}

#[tokio::test]
async fn test_range_auth_credentials_are_not_retried_after_failure() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind should succeed");
    let addr = listener.local_addr().expect("local_addr should succeed");
    let server_handle = tokio::spawn(async move {
        for expected_auth in [None, Some("Authorization: Basic d3Jvbmc6Y3JlZHM=")].iter() {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0u8; 4096];
            let bytes = stream.read(&mut request).await.expect("read request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            match expected_auth {
                Some(header) => {
                    let (_, value) = header.split_once(':').expect("test header");
                    assert!(has_header(&request, "Authorization", value.trim()));
                }
                None => assert!(!has_header_name(&request, "Authorization")),
            }
            stream
                .write_all(
                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write auth challenge");
        }
    });

    ensure_rustls_provider();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("client build should succeed");
    let auth_options = AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("wrong".to_string()),
        http_passwd: Some("creds".to_string()),
        ..AuthResolveOptions::default()
    };
    let dl = HttpSegmentDownloader::new(&client).with_auth_options(auth_options, None);
    let url = format!("http://{addr}/file.bin");

    let result = dl.download_range(&url, 0, 10, None, &[], None, 20).await;
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(
            RecoverableError::HttpAuthFailed { .. }
        ))
    ));

    tokio::time::timeout(Duration::from_secs(2), server_handle)
        .await
        .expect("failed-auth fixture should finish")
        .expect("failed-auth fixture task should succeed");
}
