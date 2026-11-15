//! Tests for HTTP proxy module.

use std::time::Duration;

use base64::{Engine, engine::general_purpose};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::http::header_processor::HttpHeaderProcessor;
use crate::http::proxy::auth::build_proxy_auth_header;
use crate::http::proxy::config::HttpProxyConfig;
use crate::http::proxy::forward::{HttpProxyForward, forward_get_with_auth};
use crate::http::proxy::response::ProxyResponse;
use crate::http::proxy::tunnel::HttpProxyTunnel;
use crate::http::socks_connector::ProxyUrl;

async fn read_request_head(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = Vec::with_capacity(512);
    loop {
        let mut byte = [0; 1];
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
        if request.ends_with(b"\r\n\r\n") {
            return request;
        }
        assert!(
            request.len() <= 16 * 1024,
            "request header exceeded test limit"
        );
    }
}

// ==================== HttpProxyConfig tests ====================

#[test]
fn test_proxy_config_new() {
    let config = HttpProxyConfig::new(
        "proxy.example.com".into(),
        3128,
        "target.example.com".into(),
        443,
    );
    assert_eq!(config.proxy_host, "proxy.example.com");
    assert_eq!(config.proxy_port, 3128);
    assert_eq!(config.target_host, "target.example.com");
    assert_eq!(config.target_port, 443);
    assert!(config.proxy_username.is_none());
    assert!(config.proxy_password.is_none());
    assert_eq!(config.connect_timeout, Duration::from_secs(30));
    assert_eq!(config.read_timeout, Duration::from_secs(60));
}

#[test]
fn test_proxy_config_with_credentials() {
    let config = HttpProxyConfig::new("p".into(), 8080, "t".into(), 80)
        .with_credentials("user".into(), "pass".into());
    assert_eq!(config.proxy_username.as_deref(), Some("user"));
    assert_eq!(config.proxy_password.as_deref(), Some("pass"));
}

#[test]
fn test_proxy_config_from_proxy_url_http() {
    let config = HttpProxyConfig::from_proxy_url(
        "http://admin:secret@proxy.corp.com:3128",
        "target.com".into(),
        443,
    )
    .unwrap();
    assert_eq!(config.proxy_host, "proxy.corp.com");
    assert_eq!(config.proxy_port, 3128);
    assert_eq!(config.proxy_username.as_deref(), Some("admin"));
    assert_eq!(config.proxy_password.as_deref(), Some("secret"));
    assert_eq!(config.target_host, "target.com");
    assert_eq!(config.target_port, 443);
}

#[test]
fn test_proxy_config_from_proxy_url_decodes_encoded_credentials() {
    let config = HttpProxyConfig::from_proxy_url(
        "http://user%40name:p%3A%40%25@[2001:db8::1]:3128",
        "target.com".into(),
        443,
    )
    .unwrap();

    assert_eq!(config.proxy_host, "2001:db8::1");
    assert_eq!(config.proxy_port, 3128);
    assert_eq!(config.proxy_username.as_deref(), Some("user@name"));
    assert_eq!(config.proxy_password.as_deref(), Some("p:@%"));
}

#[test]
fn test_proxy_config_from_proxy_url_https() {
    let config =
        HttpProxyConfig::from_proxy_url("https://secure.proxy.com", "target.com".into(), 443)
            .unwrap();
    assert_eq!(config.proxy_host, "secure.proxy.com");
    assert_eq!(config.proxy_port, 443); // default HTTPS port
}

#[test]
fn test_proxy_config_from_proxy_url_no_credentials() {
    let config =
        HttpProxyConfig::from_proxy_url("http://proxy.local:8080", "t".into(), 80).unwrap();
    assert!(config.proxy_username.is_none());
    assert!(config.proxy_password.is_none());
}

#[test]
fn test_proxy_config_from_proxy_url_socks5_accepted() {
    // SOCKS5 proxy URLs are now supported (via socks_connector)
    let result = HttpProxyConfig::from_proxy_url("socks5://proxy.local:1080", "t".into(), 80);
    assert!(
        result.is_ok(),
        "SOCKS5 proxy should be accepted: {:?}",
        result.err()
    );
    let config = result.unwrap();
    assert_eq!(config.proxy_host, "proxy.local");
    assert_eq!(config.proxy_port, 1080);
    assert!(config.is_socks());
}

#[test]
fn test_proxy_config_target_host_port() {
    let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
    assert_eq!(config.target_host_port(), "t:443");
}

// ==================== ProxyResponse tests ====================

#[test]
fn test_proxy_response_200_connected() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 Connection established\r\n\r\n");
    let head = proc.get_result().unwrap();

    let resp = ProxyResponse::from_head(head);
    match resp {
        ProxyResponse::Connected(h) => {
            assert_eq!(h.status_code, 200);
        }
        _ => panic!("Expected Connected, got {:?}", resp),
    }
}

#[test]
fn test_proxy_response_407_auth_required() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"Proxy\"\r\n\r\n");
    let head = proc.get_result().unwrap();

    let resp = ProxyResponse::from_head(head);
    match resp {
        ProxyResponse::AuthRequired { response } => {
            assert_eq!(response.status_code, 407);
            assert_eq!(
                response.header("proxy-authenticate"),
                Some("Basic realm=\"Proxy\"")
            );
        }
        _ => panic!("Expected AuthRequired, got {:?}", resp),
    }
}

#[test]
fn test_proxy_response_error_status() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 403 Forbidden\r\n\r\n");
    let head = proc.get_result().unwrap();

    let resp = ProxyResponse::from_head(head);
    match resp {
        ProxyResponse::Error {
            status_code,
            reason,
        } => {
            assert_eq!(status_code, 403);
            assert_eq!(reason, "Forbidden");
        }
        _ => panic!("Expected Error, got {:?}", resp),
    }
}

#[test]
fn test_proxy_response_500_error() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 500 Internal Server Error\r\n\r\n");
    let head = proc.get_result().unwrap();

    let resp = ProxyResponse::from_head(head);
    match resp {
        ProxyResponse::Error {
            status_code,
            reason,
        } => {
            assert_eq!(status_code, 500);
            assert_eq!(reason, "Internal Server Error");
        }
        _ => panic!("Expected Error, got {:?}", resp),
    }
}

// ==================== Auth header builder tests ====================

#[test]
fn test_proxy_basic_auth_encoding() {
    let auth = aria2_protocol::http::auth::basic_authorization("user", "pass");
    assert!(auth.starts_with("Basic "));
    // Verify Base64: "user:pass" -> "dXNlcjpwYXNz"
    assert_eq!(auth, "Basic dXNlcjpwYXNz");
}

#[test]
fn test_proxy_basic_auth_special_chars() {
    let auth = aria2_protocol::http::auth::basic_authorization("admin@corp", "p@ss:w0rd");
    assert!(auth.starts_with("Basic "));
    // Decode to verify
    let encoded = &auth["Basic ".len()..];
    let decoded = String::from_utf8(
        general_purpose::STANDARD
            .decode(encoded)
            .unwrap_or_default(),
    )
    .unwrap_or_default();
    assert_eq!(decoded, "admin@corp:p@ss:w0rd");
}

#[test]
fn test_build_proxy_auth_header_basic() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: Basic realm=\"Proxy\"\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
    assert!(auth.is_some());
    let auth = auth.unwrap();
    assert!(auth.starts_with("Basic "));
}

#[test]
fn test_build_proxy_auth_header_digest() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: Digest realm=\"Proxy\", nonce=\"abc123\", qop=\"auth\"\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
    assert!(auth.is_some());
    let auth = auth.unwrap();
    assert!(auth.starts_with("Digest "));
    assert!(auth.contains(r#"username="user""#));
}

#[test]
fn test_build_proxy_auth_header_digest_preferred_over_basic() {
    // When both Digest and Basic are offered, Digest should be preferred
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: Basic realm=\"Proxy\"\r\nProxy-Authenticate: Digest realm=\"Proxy\", nonce=\"abc123\", qop=\"auth\"\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
    assert!(auth.is_some());
    let auth = auth.unwrap();
    // Digest should be preferred over Basic
    assert!(auth.starts_with("Digest "));
}

#[test]
fn test_build_proxy_auth_header_handles_mixed_case_digest_scheme() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: dIgEsT realm=\"proxy, internal\", nonce=\"n1\", qop=\"auth-int\", algorithm=SHA-256\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1)
        .expect("mixed-case Digest challenge should be recognized");
    assert!(auth.starts_with("Digest "));
    assert!(auth.contains("realm=\"proxy, internal\""));
    assert!(auth.contains("qop=auth-int"));
    assert!(auth.contains("algorithm=SHA-256"));
}

#[test]
fn test_build_proxy_auth_header_no_auth_header() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
    // No Proxy-Authenticate header -> None
    assert!(auth.is_none());
}

// ==================== HttpProxyForward request building tests ====================

#[test]
fn test_forward_build_request_no_auth() {
    let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 80);
    let forward = HttpProxyForward::new(config);
    let req = forward.build_forward_request("GET", "http://target.com:80/path", "/path", None);

    assert!(req.starts_with("GET http://target.com:80/path HTTP/1.1\r\n"));
    assert!(req.contains("Host: target.com:80\r\n"));
    assert!(!req.contains("Proxy-Authorization"));
    assert!(req.contains(&format!("User-Agent: {}\r\n", crate::constants::USER_AGENT)));
    assert!(req.ends_with("\r\n\r\n"));
}

#[test]
fn test_forward_build_request_with_auth() {
    let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 80);
    let forward = HttpProxyForward::new(config);
    let req = forward.build_forward_request(
        "GET",
        "http://target.com:80/file.zip",
        "/file.zip",
        Some("Basic dXNlcjpwYXNz"),
    );

    assert!(req.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
}

#[test]
fn test_forward_build_head_request() {
    let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 80);
    let forward = HttpProxyForward::new(config);
    let req = forward.build_forward_request("HEAD", "http://target.com:80/", "/", None);

    assert!(req.starts_with("HEAD http://target.com:80/ HTTP/1.1\r\n"));
}

#[test]
fn test_forward_get_credentials() {
    let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 80)
        .with_credentials("admin".into(), "s3cret".into());
    let forward = HttpProxyForward::new(config);
    let (u, p) = forward.get_credentials().unwrap();
    assert_eq!(u, "admin");
    assert_eq!(p, "s3cret");
}

// ==================== proxy_digest_auth tests ====================

#[test]
fn test_proxy_digest_auth_produces_header() {
    let challenge = aria2_protocol::http::digest::DigestChallenge::parse(
        r#"Digest realm="Proxy", nonce="abc123", qop="auth""#,
    )
    .unwrap();
    let auth = aria2_protocol::http::digest::DigestResponse::compute(
        "user",
        "pass",
        "CONNECT",
        "target.com:443",
        &challenge,
        1,
    )
    .to_header_value();
    assert!(auth.starts_with("Digest "));
    assert!(auth.contains(r#"username="user""#));
    assert!(auth.contains(r#"realm="Proxy""#));
    assert!(auth.contains("nc=00000001"));
}

// ==================== ProxyUrl integration tests ====================

#[test]
fn test_proxy_url_http_default_port() {
    let parsed = ProxyUrl::parse("http://proxy.local").unwrap();
    assert_eq!(parsed.port, 8080);
}

#[test]
fn test_proxy_url_https_default_port() {
    let parsed = ProxyUrl::parse("https://proxy.local").unwrap();
    assert_eq!(parsed.port, 443);
}

#[test]
fn test_proxy_url_with_auth() {
    let parsed = ProxyUrl::parse("http://u:p@proxy.local:3128").unwrap();
    assert_eq!(parsed.username, Some("u".to_string()));
    assert_eq!(parsed.password, Some("p".to_string()));
}

// ==================== Timeout configuration tests ====================

#[test]
fn test_custom_timeouts() {
    let mut config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
    config.connect_timeout = Duration::from_secs(10);
    config.read_timeout = Duration::from_secs(30);
    config.write_timeout = Duration::from_secs(30);
    assert_eq!(config.connect_timeout, Duration::from_secs(10));
    assert_eq!(config.read_timeout, Duration::from_secs(30));
    assert_eq!(config.write_timeout, Duration::from_secs(30));
}

// ==================== Edge case tests ====================

#[test]
fn test_proxy_response_non_standard_2xx() {
    // Some proxies return 200 with different reason phrases
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 200 Tunnel Connection Established\r\n\r\n");
    let head = proc.get_result().unwrap();

    let resp = ProxyResponse::from_head(head);
    assert!(matches!(resp, ProxyResponse::Connected(_)));
}

#[test]
fn test_build_proxy_auth_header_fallback_to_basic() {
    // Unknown scheme should fall back to Basic
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\nProxy-Authenticate: NTLM\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
    assert!(auth.is_some());
    let auth = auth.unwrap();
    // Should fall back to Basic
    assert!(auth.starts_with("Basic "));
}

#[tokio::test]
async fn public_connect_tunnel_uses_protocol_handshake_and_preserves_request() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request_head(&mut stream).await;
        assert_eq!(
            request.as_slice(),
            b"CONNECT target.example:443 HTTP/1.1\r\nHost: target.example:443\r\nProxy-Connection: keep-alive\r\n\r\n"
        );
        stream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
    });

    let mut config = HttpProxyConfig::new(
        "127.0.0.1".into(),
        address.port(),
        "target.example".into(),
        443,
    );
    config.connect_timeout = Duration::from_secs(2);
    config.read_timeout = Duration::from_secs(2);
    config.write_timeout = Duration::from_secs(2);
    let tunnel = HttpProxyTunnel::new(config);
    let stream = tunnel.connect().await.unwrap();
    drop(stream);
    server.await.unwrap();
}

#[test]
fn test_forward_request_target_port_80() {
    let config = HttpProxyConfig::new("proxy.local".into(), 8080, "example.com".into(), 80);
    let forward = HttpProxyForward::new(config);
    let req = forward.build_forward_request(
        "GET",
        "http://example.com:80/index.html",
        "/index.html",
        None,
    );

    assert!(req.starts_with("GET http://example.com:80/index.html HTTP/1.1\r\n"));
    assert!(req.contains("Host: example.com:80\r\n"));
}

#[tokio::test]
async fn forward_get_authentication_preserves_response_body_after_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let request = read_request_head(&mut first).await;
        assert!(request.starts_with(b"GET http://example.com:80/file HTTP/1.1\r\n"));
        first
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();
        drop(first);

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_request_head(&mut second).await;
        assert!(
            request
                .windows(b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n".len())
                .any(|window| window == b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n")
        );
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nBODY")
            .await
            .unwrap();
    });

    let mut config =
        HttpProxyConfig::new("127.0.0.1".into(), address.port(), "example.com".into(), 80)
            .with_credentials("user".into(), "pass".into());
    config.connect_timeout = Duration::from_secs(2);
    config.read_timeout = Duration::from_secs(2);
    config.write_timeout = Duration::from_secs(2);

    let (mut stream, response) = forward_get_with_auth(&config, "/file").await.unwrap();
    assert_eq!(response.status_code, 200);
    let mut body = [0; 4];
    stream.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, b"BODY");
    server.await.unwrap();
}

#[tokio::test]
async fn forward_connect_authenticates_probe_then_returns_fresh_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let request = read_request_head(&mut first).await;
        assert!(request.starts_with(b"HEAD http://example.com:80 HTTP/1.1\r\n"));
        assert!(
            !request
                .windows(b"Proxy-Authorization:".len())
                .any(|window| window == b"Proxy-Authorization:")
        );
        first
            .write_all(
                b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();
        drop(first);

        let (mut second, _) = listener.accept().await.unwrap();
        let request = read_request_head(&mut second).await;
        assert!(
            request
                .windows(b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n".len())
                .any(|window| window == b"Proxy-Authorization: Basic dXNlcjpwYXNz\r\n")
        );
        second
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        drop(second);

        listener.accept().await.unwrap()
    });

    let mut config =
        HttpProxyConfig::new("127.0.0.1".into(), address.port(), "example.com".into(), 80)
            .with_credentials("user".into(), "pass".into());
    config.connect_timeout = Duration::from_secs(2);
    config.read_timeout = Duration::from_secs(2);
    config.write_timeout = Duration::from_secs(2);

    let stream = HttpProxyForward::new(config).connect(false).await.unwrap();
    drop(stream);
    server.await.unwrap();
}
