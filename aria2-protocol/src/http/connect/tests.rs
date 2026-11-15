use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

fn options<'a>(user_agent: &'a str) -> HttpConnectOptions<'a> {
    HttpConnectOptions {
        target_host: "example.com",
        target_port: 443,
        user_agent,
        username: None,
        password: None,
        preemptive_basic_auth: false,
        write_timeout: Duration::from_secs(2),
        read_timeout: Duration::from_secs(2),
    }
}

#[test]
fn proxy_auth_body_framing_requires_unambiguous_http_boundaries() {
    assert_eq!(
        proxy_response_body_framing(&[(
            "Transfer-Encoding".to_string(),
            "gzip, chunked".to_string(),
        )])
        .unwrap(),
        ProxyResponseBodyFraming::Chunked
    );
    assert!(matches!(
        proxy_response_body_framing(&[(
            "Transfer-Encoding".to_string(),
            "chunked, gzip".to_string(),
        )]),
        Err(HttpConnectError::InvalidResponse(_))
    ));
    assert!(matches!(
        proxy_response_body_framing(&[(
            "Transfer-Encoding".to_string(),
            "chunked, chunked".to_string(),
        )]),
        Err(HttpConnectError::InvalidResponse(_))
    ));
    assert!(matches!(
        proxy_response_body_framing(&[
            ("Content-Length".to_string(), "1".to_string()),
            ("content-length".to_string(), "2".to_string()),
        ]),
        Err(HttpConnectError::InvalidResponse(_))
    ));
    assert!(matches!(
        proxy_response_body_framing(&[("Content-Length".to_string(), "not-a-length".to_string(),)]),
        Err(HttpConnectError::InvalidResponse(_))
    ));
    assert!(matches!(
        proxy_response_body_framing(&[]),
        Err(HttpConnectError::InvalidResponse(_))
    ));
    assert_eq!(
        proxy_response_body_framing(&[("Content-Length".to_string(), "1, 1".to_string(),)])
            .unwrap(),
        ProxyResponseBodyFraming::ContentLength(1)
    );
}

#[tokio::test]
async fn skips_informational_responses_and_preserves_tunnel_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let length = stream.read(&mut request).await.unwrap();
        assert!(request[..length].starts_with(b"CONNECT example.com:443 HTTP/1.1\r\n"));
        stream
            .write_all(
                b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Connection Established\r\n\r\nTLS",
            )
            .await
            .unwrap();
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    establish_connect_tunnel(&mut stream, &options("test-agent"))
        .await
        .unwrap();
    let mut tunneled = [0; 3];
    stream.read_exact(&mut tunneled).await.unwrap();
    assert_eq!(&tunneled, b"TLS");
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_user_agent_is_rejected_before_connect_request_is_written() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 256];
        let result =
            tokio::time::timeout(Duration::from_millis(100), stream.read(&mut request)).await;
        assert!(result.is_err(), "invalid request must not be written");
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let options = HttpConnectOptions {
        user_agent: "agent\r\nX-Injected: yes",
        ..options("unused")
    };
    assert!(matches!(
        establish_connect_tunnel(&mut stream, &options).await,
        Err(HttpConnectError::InvalidRequestField("User-Agent"))
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn challenge_authentication_is_sent_only_after_407() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        let length = stream.read(&mut request).await.unwrap();
        assert!(
            !request[..length]
                .windows(b"Proxy-Authorization:".len())
                .any(|window| window == b"Proxy-Authorization:")
        );
        stream
                .write_all(
                    b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 1\r\n\r\nx",
                )
                .await
                .unwrap();
        let length = stream.read(&mut request).await.unwrap();
        assert!(
            request[..length]
                .windows(b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n".len())
                .any(|window| window == b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n")
        );
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let options = HttpConnectOptions {
        username: Some("user"),
        password: Some("pass"),
        ..options("test-agent")
    };
    establish_connect_tunnel(&mut stream, &options)
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn unframed_407_stops_before_authenticated_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        let length = stream.read(&mut request).await.unwrap();
        assert!(request[..length].starts_with(b"CONNECT example.com:443 HTTP/1.1\r\n"));
        stream
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\n\r\n",
            )
            .await
            .unwrap();

        let mut retry = [0; 2048];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.read(&mut retry))
                .await
                .is_err(),
            "an unframed 407 must not trigger an authenticated retry"
        );
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let options = HttpConnectOptions {
        username: Some("user"),
        password: Some("pass"),
        ..options("test-agent")
    };
    let result = establish_connect_tunnel(&mut stream, &options).await;
    assert!(matches!(
        result,
        Err(HttpConnectError::InvalidResponse(message))
            if message.contains("no body framing")
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn proxy_auth_body_drain_preserves_bytes_after_content_length() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream.write_all(b"xNEXT").await.unwrap();
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let headers = vec![("content-length".to_string(), "1".to_string())];
    consume_response_body(&mut stream, &headers, Duration::from_secs(2))
        .await
        .unwrap();
    let mut remaining = [0; 4];
    stream.read_exact(&mut remaining).await.unwrap();
    assert_eq!(&remaining, b"NEXT");
    server.await.unwrap();
}

#[tokio::test]
async fn proxy_auth_body_drain_uses_configured_read_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        stream.write_all(b"x").await.unwrap();
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let headers = vec![("content-length".to_string(), "1".to_string())];
    let result = consume_response_body(&mut stream, &headers, Duration::from_millis(10)).await;

    assert!(matches!(result, Err(HttpConnectError::Timeout)));
    server.await.unwrap();
}

#[tokio::test]
async fn proxy_auth_chunked_body_drain_preserves_bytes_after_body() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        stream
            .write_all(b"1\r\nx\r\n0\r\nTrailer: done\r\n\r\nNEXT")
            .await
            .unwrap();
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let headers = vec![("transfer-encoding".to_string(), "chunked".to_string())];
    consume_response_body(&mut stream, &headers, Duration::from_secs(2))
        .await
        .unwrap();
    let mut remaining = [0; 4];
    stream.read_exact(&mut remaining).await.unwrap();
    assert_eq!(&remaining, b"NEXT");
    server.await.unwrap();
}

#[tokio::test]
async fn preemptive_basic_auth_is_sent_on_the_first_request_when_enabled() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 2048];
        let length = stream.read(&mut request).await.unwrap();
        assert!(
            request[..length]
                .windows(b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n".len())
                .any(|window| window == b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n")
        );
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
    });

    let mut stream = TcpStream::connect(address).await.unwrap();
    let options = HttpConnectOptions {
        username: Some("user"),
        password: Some("pass"),
        preemptive_basic_auth: true,
        ..options("test-agent")
    };
    establish_connect_tunnel(&mut stream, &options)
        .await
        .unwrap();
    server.await.unwrap();
}

#[test]
fn md5_sess_connect_response_uses_client_nonce_in_session_hash() {
    let challenge = DigestChallenge::parse(
            r#"Digest realm="testrealm@host.com", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", qop="auth", algorithm=MD5-sess"#,
        )
        .unwrap();
    let response = DigestResponse::compute_with_cnonce(
        "Mufasa",
        "Circle Of Life",
        "CONNECT",
        "target.example.com:443",
        &challenge,
        1,
        "aria2rustcnonce",
    )
    .to_header_value();

    assert!(response.contains("cnonce=\"aria2rustcnonce\""));
    assert!(response.contains("response=\"3ac03d7149007bfa5959a930308906e0\""));
}

#[test]
fn compatibility_policy_prefers_digest_and_uses_retry_count() {
    let headers = vec![
        (
            "proxy-authenticate".to_string(),
            "Basic realm=\"test\"".to_string(),
        ),
        (
            "proxy-authenticate".to_string(),
            "Digest realm=\"test\", nonce=\"n1\", qop=\"auth\"".to_string(),
        ),
    ];
    let auth = challenged_authentication(
        "user",
        "pass",
        &headers,
        "example.com:443",
        2,
        &HttpConnectRetryPolicy {
            max_auth_retries: 2,
            prefer_digest: true,
            fallback_to_basic: true,
            proxy_connection_keep_alive: true,
        },
    )
    .unwrap();

    assert!(auth.starts_with("Digest "));
    assert!(auth.contains("nc=00000002"));
}

#[test]
fn compatibility_policy_falls_back_to_basic_for_unknown_challenge() {
    let headers = vec![("proxy-authenticate".to_string(), "NTLM".to_string())];
    let auth = challenged_authentication(
        "user",
        "pass",
        &headers,
        "example.com:443",
        1,
        &HttpConnectRetryPolicy {
            fallback_to_basic: true,
            ..HttpConnectRetryPolicy::default()
        },
    )
    .unwrap();

    assert_eq!(auth, "Basic dXNlcjpwYXNz");
}
