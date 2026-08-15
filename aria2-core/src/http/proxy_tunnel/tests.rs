//! Tests for the proxy_tunnel module

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::header_processor::HttpHeaderProcessor;
use crate::http::request_response::basic_auth;

use super::auth;
use super::connect::HttpProxyTunnel;
use super::*;

fn test_config() -> HttpProxyTunnelConfig {
    HttpProxyTunnelConfig {
        proxy_host: "proxy.example.com".to_string(),
        proxy_port: 8080,
        username: None,
        password: None,
        target_host: "target.example.com".to_string(),
        target_port: 443,
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(5),
        write_timeout: Duration::from_secs(5),
        user_agent: crate::constants::USER_AGENT.to_string(),
    }
}

#[test]
fn test_connect_request_without_auth() {
    let config = test_config();
    let request = HttpProxyTunnel::build_connect_request(&config, None);
    assert!(request.starts_with("CONNECT target.example.com:443 HTTP/1.1\r\n"));
    assert!(request.contains(&format!("User-Agent: {}\r\n", crate::constants::USER_AGENT)));
    assert!(request.contains("Host: target.example.com:443\r\n"));
    assert!(!request.contains("Proxy-Authorization"));
    assert!(request.ends_with("\r\n\r\n"));
}

#[test]
fn test_connect_request_with_basic_auth() {
    let config = test_config();
    let auth = basic_auth("user", "pass");
    let request = HttpProxyTunnel::build_connect_request(&config, Some(&auth));
    assert!(request.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
}

#[test]
fn test_connect_request_format() {
    let config = test_config();
    let request = HttpProxyTunnel::build_connect_request(&config, None);
    let lines: Vec<&str> = request.trim_end().split("\r\n").collect();
    assert!(lines[0].starts_with("CONNECT "));
    assert!(lines[0].ends_with(" HTTP/1.1"));
}

#[test]
fn test_forward_request_line_get() {
    assert_eq!(
        HttpProxyTunnel::build_forward_request_line("GET", "http://t.com/p"),
        "GET http://t.com/p HTTP/1.1\r\n"
    );
}

#[test]
fn test_basic_auth_header() {
    assert_eq!(basic_auth("user", "pass"), "Basic dXNlcjpwYXNz");
}

#[test]
fn test_digest_auth_header() {
    let challenge = r#"Digest realm="test@ex.com", nonce="abc", qop="auth", algorithm=MD5"#;
    let header = auth::build_digest_auth_header("user", "pass", challenge, "t:443");
    assert!(header.starts_with("Digest"));
    assert!(header.contains("username=\"user\""));
    assert!(header.contains("realm=\"test@ex.com\""));
    assert!(header.contains("response=\""));
}

#[test]
fn test_digest_auth_fallback_on_bad_challenge() {
    let header = auth::build_digest_auth_header("u", "p", "NotDigest", "t:443");
    assert!(header.starts_with("Basic "));
}

#[test]
fn test_preemptive_auth_with_credentials() {
    let config = HttpProxyTunnelConfig {
        username: Some("u".into()),
        password: Some("p".into()),
        ..test_config()
    };
    assert!(auth::maybe_preemptive_basic_auth(&config).is_some());
}

#[test]
fn test_preemptive_auth_without_credentials() {
    assert!(auth::maybe_preemptive_basic_auth(&test_config()).is_none());
}

#[test]
fn test_preemptive_auth_empty_username() {
    let config = HttpProxyTunnelConfig {
        username: Some(String::new()),
        password: Some("p".into()),
        ..test_config()
    };
    assert!(auth::maybe_preemptive_basic_auth(&config).is_none());
}

#[test]
fn test_parse_200_response() {
    let raw = b"HTTP/1.1 200 Connection Established\r\n\r\n";
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(raw);
    assert_eq!(proc.get_result().unwrap().status_code, 200);
}

#[test]
fn test_parse_407_response() {
    let raw = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"proxy\"\r\n\r\n";
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(raw);
    let head = proc.get_result().unwrap();
    assert_eq!(head.status_code, 407);
    assert_eq!(
        head.header("proxy-authenticate"),
        Some("Basic realm=\"proxy\"")
    );
}

#[test]
fn test_config_default() {
    let config = HttpProxyTunnelConfig::default();
    assert_eq!(config.proxy_port, 8080);
    assert_eq!(config.target_port, 80);
    assert!(config.username.is_none());
}

#[test]
fn test_md5_hex_known_values() {
    assert_eq!(auth::md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
    assert_eq!(auth::md5_hex("hello"), "5d41402abc4b2a76b9719d911017c592");
}

#[test]
fn test_proxy_type_variants() {
    assert_eq!(HttpProxyType::Tunnel, HttpProxyType::Tunnel);
    assert_ne!(HttpProxyType::Tunnel, HttpProxyType::Forward);
}

// =======================================================================
// Mock-server integration tests
// =======================================================================

async fn start_mock_proxy() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

fn config_for_mock(port: u16) -> HttpProxyTunnelConfig {
    HttpProxyTunnelConfig {
        proxy_host: "127.0.0.1".to_string(),
        proxy_port: port,
        target_host: "target.example.com".to_string(),
        target_port: 443,
        connect_timeout: Duration::from_secs(5),
        read_timeout: Duration::from_secs(5),
        write_timeout: Duration::from_secs(5),
        user_agent: "test-proxy-client/1.0".to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn test_tunnel_success_200() {
    let (listener, port) = start_mock_proxy().await;
    let config = config_for_mock(port);
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let n = AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
        let req = String::from_utf8_lossy(&buf[..n]);
        assert!(req.starts_with("CONNECT target.example.com:443"));
        AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_ok(), "Expected tunnel success, got: {:?}", result);
    assert_eq!(result.unwrap().proxy_type, HttpProxyType::Tunnel);
    server.await.unwrap();
}

#[tokio::test]
async fn test_tunnel_rejected_non_200() {
    let (listener, port) = start_mock_proxy().await;
    let config = config_for_mock(port);
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
        AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 403 Forbidden\r\n\r\n")
            .await
            .unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("403"), "Expected 403, got: {}", msg);
    assert!(
        msg.contains("rejected CONNECT"),
        "Expected 'rejected CONNECT', got: {}",
        msg
    );
    server.await.unwrap();
}

#[tokio::test]
async fn test_tunnel_407_basic_auth_success() {
    let (listener, port) = start_mock_proxy().await;
    let config = HttpProxyTunnelConfig {
        username: Some("user".into()),
        password: Some("pass".into()),
        ..config_for_mock(port)
    };
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 8192];
        let n = AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
        let _req1 = String::from_utf8_lossy(&buf[..n]);
        AsyncWriteExt::write_all(&mut sock,
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n"
        ).await.unwrap();
        let n2 = AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
        let req2 = String::from_utf8_lossy(&buf[..n2]);
        assert!(req2.contains("Proxy-Authorization: Basic dXNlcjpwYXNz"));
        AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_ok(), "Expected auth success, got: {:?}", result);
    assert_eq!(result.unwrap().proxy_type, HttpProxyType::Tunnel);
    server.await.unwrap();
}

#[tokio::test]
async fn test_tunnel_407_no_credentials_fails() {
    let (listener, port) = start_mock_proxy().await;
    let config = config_for_mock(port);
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
        AsyncWriteExt::write_all(&mut sock,
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n"
        ).await.unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("auth"));
    server.await.unwrap();
}

#[tokio::test]
async fn test_tunnel_407_wrong_credentials_fails() {
    let (listener, port) = start_mock_proxy().await;
    let config = HttpProxyTunnelConfig {
        username: Some("wrong".into()),
        password: Some("creds".into()),
        ..config_for_mock(port)
    };
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
        AsyncWriteExt::write_all(&mut sock,
            b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"test\"\r\nContent-Length: 0\r\n\r\n"
        ).await.unwrap();
        let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
        AsyncWriteExt::write_all(
            &mut sock,
            b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n",
        )
        .await
        .unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("auth failed"));
    server.await.unwrap();
}

#[tokio::test]
async fn test_forward_mode_returns_stream() {
    let (listener, port) = start_mock_proxy().await;
    let config = config_for_mock(port);
    let server = tokio::spawn(async move {
        let (_sock, _) = listener.accept().await.unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Forward).await;
    assert!(
        result.is_ok(),
        "Expected forward success, got: {:?}",
        result
    );
    assert_eq!(result.unwrap().proxy_type, HttpProxyType::Forward);
    server.await.unwrap();
}

#[tokio::test]
async fn test_tunnel_1xx_then_200() {
    let (listener, port) = start_mock_proxy().await;
    let config = config_for_mock(port);
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 4096];
        let _ = AsyncReadExt::read(&mut sock, &mut buf).await;
        // Send both responses; the client's read_proxy_response must
        // handle them even if they arrive in the same TCP segment.
        AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .unwrap();
        AsyncWriteExt::write_all(&mut sock, b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(
        result.is_ok(),
        "Expected success after 1xx skip, got: {:?}",
        result
    );
    server.await.unwrap();
}

#[tokio::test]
async fn test_tunnel_connection_refused() {
    let config = HttpProxyTunnelConfig {
        proxy_host: "127.0.0.1".into(),
        proxy_port: 1,
        target_host: "t.example.com".into(),
        target_port: 443,
        connect_timeout: Duration::from_millis(500),
        ..Default::default()
    };
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_tunnel_timeout() {
    let (listener, port) = start_mock_proxy().await;
    let config = HttpProxyTunnelConfig {
        read_timeout: Duration::from_millis(200),
        ..config_for_mock(port)
    };
    let server = tokio::spawn(async move {
        let (_sock, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(10)).await;
    });
    let result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel).await;
    assert!(result.is_err(), "Expected timeout, got: {:?}", result);
    server.abort();
    let _ = server.await;
}
