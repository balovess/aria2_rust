use super::*;

#[tokio::test]
async fn proxy_get_binding_reaches_the_proxy_socket() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("proxy fixture should bind");
    let address = listener.local_addr().expect("proxy fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("proxy should accept");
        let mut request = [0u8; 4096];
        let bytes = stream.read(&mut request).await.expect("read proxy request");
        assert!(bytes > 0, "proxy request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("write proxy response");
        peer.ip()
    });

    let proxy = FtpProxyConfig {
        proxy_host: "127.0.0.1".to_string(),
        proxy_port: address.port(),
        ..FtpProxyConfig::default()
    };
    let response = execute_proxy_get_with_policy(
        Url::parse("ftp://ftp.example.com/file.bin").expect("valid FTP URL"),
        &proxy,
        0,
        false,
        &OutboundNetworkPolicy::single(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)),
    )
    .await
    .expect("proxy GET should connect with the configured source address");

    assert_eq!(response.head.status_code, 200);
    assert_eq!(
        server.await.expect("proxy fixture should finish"),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

/// Helper: build request and return as string for assertions.
fn build_request_string(ftp_url: Url, proxy_config: FtpProxyConfig) -> String {
    let result = FtpProxyGetRequestBuilder::new(ftp_url, proxy_config)
        .build()
        .unwrap();
    String::from_utf8(result.request_bytes).unwrap()
}

#[test]
fn test_proxy_method_default() {
    assert_eq!(ProxyMethod::default(), ProxyMethod::Get);
}

#[test]
fn test_proxy_method_display() {
    assert_eq!(format!("{}", ProxyMethod::Get), "GET");
    assert_eq!(format!("{}", ProxyMethod::Tunnel), "TUNNEL");
}

#[test]
fn test_resolve_proxy_method_default_ftp() {
    let config = FtpProxyConfig::default();
    assert_eq!(resolve_proxy_method(&config, "ftp"), ProxyMethod::Get);
}

#[test]
fn test_resolve_proxy_method_explicit_tunnel() {
    let config = FtpProxyConfig {
        explicit_proxy_method: Some(ProxyMethod::Tunnel),
        ..Default::default()
    };
    assert_eq!(resolve_proxy_method(&config, "ftp"), ProxyMethod::Tunnel);
}

#[test]
fn test_resolve_proxy_method_https_always_tunnel() {
    let config = FtpProxyConfig::default();
    assert_eq!(resolve_proxy_method(&config, "https"), ProxyMethod::Tunnel);
}

#[test]
fn test_resolve_proxy_method_sftp_always_tunnel() {
    let config = FtpProxyConfig::default();
    assert_eq!(resolve_proxy_method(&config, "sftp"), ProxyMethod::Tunnel);
}

#[test]
fn test_resolve_proxy_method_https_ignores_explicit_get() {
    // Even if someone explicitly sets GET, HTTPS always uses Tunnel
    let config = FtpProxyConfig {
        explicit_proxy_method: Some(ProxyMethod::Get),
        ..Default::default()
    };
    assert_eq!(resolve_proxy_method(&config, "https"), ProxyMethod::Tunnel);
}

#[test]
fn test_ftp_proxy_config_default() {
    let config = FtpProxyConfig::default();
    assert_eq!(config.proxy_port, 8080);
    assert!(config.proxy_username.is_empty());
    assert!(config.proxy_password.is_empty());
    assert!(config.ftp_username.is_empty());
    assert!(config.explicit_proxy_method.is_none());
}

#[test]
fn test_build_request_simple() {
    let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
    let proxy_config = FtpProxyConfig {
        proxy_host: "proxy.example.com".to_string(),
        proxy_port: 8080,
        user_agent: "test-client/1.0".to_string(),
        ..Default::default()
    };
    let s = build_request_string(ftp_url, proxy_config);
    assert!(s.starts_with("GET ftp://ftp.example.com/pub/file.tar.gz HTTP/1.1\r\n"));
    assert!(s.contains("Host: ftp.example.com\r\n"));
    assert!(s.contains("User-Agent: test-client/1.0\r\n"));
    assert!(s.contains("Connection: Keep-Alive\r\n"));
    assert!(!s.contains("Proxy-Authorization"));
    assert!(!s.contains("Range:"));
    assert!(s.ends_with("\r\n\r\n"));
}

#[test]
fn test_build_request_with_proxy_auth() {
    let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
    let proxy_config = FtpProxyConfig {
        proxy_username: "proxyuser".to_string(),
        proxy_password: "proxypass".to_string(),
        ..Default::default()
    };
    let s = build_request_string(ftp_url, proxy_config);
    assert!(s.contains("Proxy-Authorization: Basic cHJveHl1c2VyOnByb3h5cGFzcw==\r\n"));
}

#[test]
fn test_build_request_with_ftp_username_embedded() {
    let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
    let proxy_config = FtpProxyConfig {
        ftp_username: "ftpuser".to_string(),
        ..Default::default()
    };
    let s = build_request_string(ftp_url, proxy_config);
    assert!(s.contains("GET ftp://ftpuser@ftp.example.com/pub/file.tar.gz HTTP/1.1\r\n"));
}

#[test]
fn test_build_request_no_double_username() {
    let ftp_url = Url::parse("ftp://existing@ftp.example.com/pub/file.tar.gz").unwrap();
    let proxy_config = FtpProxyConfig {
        ftp_username: "ftpuser".to_string(),
        ..Default::default()
    };
    let s = build_request_string(ftp_url, proxy_config);
    assert!(s.contains("ftp://existing@ftp.example.com"));
    assert!(!s.contains("ftp://ftpuser@"));
}

#[test]
fn test_build_request_with_range() {
    let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
    let result = FtpProxyGetRequestBuilder::new(ftp_url, FtpProxyConfig::default())
        .range_start(1024)
        .build()
        .unwrap();
    let s = String::from_utf8(result.request_bytes).unwrap();
    assert!(s.contains("Range: bytes=1024-\r\n"));
}

#[test]
fn test_build_request_with_no_cache() {
    let ftp_url = Url::parse("ftp://ftp.example.com/pub/file.tar.gz").unwrap();
    let result = FtpProxyGetRequestBuilder::new(ftp_url, FtpProxyConfig::default())
        .no_cache(true)
        .build()
        .unwrap();
    let s = String::from_utf8(result.request_bytes).unwrap();
    assert!(s.contains("Pragma: no-cache\r\n"));
    assert!(s.contains("Cache-Control: no-cache\r\n"));
}

#[test]
fn test_build_request_full_round_trip() {
    let ftp_url = Url::parse("ftp://ftp.example.com/pub/linux/file.tar.gz").unwrap();
    let proxy_config = FtpProxyConfig {
        proxy_host: "proxy.local".to_string(),
        proxy_port: 3128,
        proxy_username: "puser".to_string(),
        proxy_password: "ppass".to_string(),
        ftp_username: "anonymous".to_string(),
        user_agent: "test-client/2.0".to_string(),
        ..Default::default()
    };

    let result = FtpProxyGetRequestBuilder::new(ftp_url, proxy_config)
        .range_start(4096)
        .no_cache(true)
        .build()
        .unwrap();

    let s = String::from_utf8(result.request_bytes).unwrap();
    assert!(s.contains("GET ftp://anonymous@ftp.example.com/pub/linux/file.tar.gz HTTP/1.1\r\n"));
    assert!(s.contains("Host: ftp.example.com\r\n"));
    assert!(s.contains("User-Agent: test-client/2.0\r\n"));
    assert!(s.contains("Pragma: no-cache\r\n"));
    assert!(s.contains("Connection: Keep-Alive\r\n"));
    assert!(s.contains("Range: bytes=4096-\r\n"));
    assert!(s.contains("Proxy-Authorization: Basic"));
    assert!(s.ends_with("\r\n\r\n"));
    assert_eq!(
        result.request_url,
        "ftp://anonymous@ftp.example.com/pub/linux/file.tar.gz"
    );
}
