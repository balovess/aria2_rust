//! Tests for the proxy_tunnel module

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::request::basic_auth;

use super::*;

#[test]
fn test_basic_auth_header() {
    assert_eq!(basic_auth("user", "pass"), "Basic dXNlcjpwYXNz");
}

#[test]
fn test_config_default() {
    let config = HttpProxyTunnelConfig::default();
    assert_eq!(config.proxy_port, 8080);
    assert_eq!(config.target_port, 80);
    assert!(config.username.is_none());
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
        // The final response and the first tunneled bytes may share a packet.
        AsyncWriteExt::write_all(
            &mut sock,
            b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 Connection Established\r\n\r\n220 FTP ready\r\n",
        )
            .await
            .unwrap();
    });
    let mut result = establish_http_proxy_tunnel(&config, HttpProxyType::Tunnel)
        .await
        .expect("success after skipping 1xx response");
    let mut greeting = [0; 15];
    result.stream.read_exact(&mut greeting).await.unwrap();
    assert_eq!(&greeting, b"220 FTP ready\r\n");
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
