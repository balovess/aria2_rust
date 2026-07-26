//! Tests for HTTP proxy module.

use std::time::Duration;

use base64::{Engine, engine::general_purpose};

use crate::http::digest_auth::DigestAuthChallenge;
use crate::http::header_processor::HttpHeaderProcessor;
use crate::http::proxy::auth::{build_proxy_auth_header, proxy_basic_auth, proxy_digest_auth};
use crate::http::proxy::config::HttpProxyConfig;
use crate::http::proxy::forward::HttpProxyForward;
use crate::http::proxy::response::ProxyResponse;
use crate::http::proxy::tunnel::HttpProxyTunnel;
use crate::http::socks_connector::ProxyUrl;

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
    let auth = proxy_basic_auth("user", "pass");
    assert!(auth.starts_with("Basic "));
    // Verify Base64: "user:pass" -> "dXNlcjpwYXNz"
    assert_eq!(auth, "Basic dXNlcjpwYXNz");
}

#[test]
fn test_proxy_basic_auth_special_chars() {
    let auth = proxy_basic_auth("admin@corp", "p@ss:w0rd");
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
fn test_build_proxy_auth_header_no_auth_header() {
    let mut proc = HttpHeaderProcessor::new();
    proc.feed(b"HTTP/1.1 407 Auth\r\n\r\n");
    let head = proc.get_result().unwrap();

    let auth = build_proxy_auth_header(&head, "user", "pass", "CONNECT", "t:443", 1);
    // No Proxy-Authenticate header -> None
    assert!(auth.is_none());
}

// ==================== HttpProxyTunnel request building tests ====================

#[test]
fn test_tunnel_build_connect_request_no_auth() {
    let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 443);
    let tunnel = HttpProxyTunnel::new(config);
    let req = tunnel.build_connect_request(None);

    assert!(req.starts_with("CONNECT target.com:443 HTTP/1.1\r\n"));
    assert!(req.contains("Host: target.com:443\r\n"));
    assert!(req.contains("Proxy-Connection: keep-alive\r\n"));
    assert!(!req.contains("Proxy-Authorization"));
    assert!(req.ends_with("\r\n\r\n"));
}

#[test]
fn test_tunnel_build_connect_request_with_basic_auth() {
    let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 443);
    let tunnel = HttpProxyTunnel::new(config);
    let req = tunnel.build_connect_request(Some("Basic dXNlcjpwYXNz"));

    assert!(req.contains("Proxy-Authorization: Basic dXNlcjpwYXNz\r\n"));
}

#[test]
fn test_tunnel_build_connect_request_with_digest_auth() {
    let config = HttpProxyConfig::new("proxy.com".into(), 3128, "target.com".into(), 443);
    let tunnel = HttpProxyTunnel::new(config);
    let digest_value = r#"Digest username="admin", realm="Proxy", nonce="abc", uri="target.com:443", nc=00000001, cnonce="x", qop="auth", response="h", algorithm="MD5", opaque="o""#;
    let req = tunnel.build_connect_request(Some(digest_value));

    assert!(req.contains("Proxy-Authorization: Digest "));
    assert!(req.contains(r#"username="admin""#));
}

#[test]
fn test_tunnel_get_credentials() {
    let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443)
        .with_credentials("user".into(), "pass".into());
    let tunnel = HttpProxyTunnel::new(config);
    let (u, p) = tunnel.get_credentials().unwrap();
    assert_eq!(u, "user");
    assert_eq!(p, "pass");
}

#[test]
fn test_tunnel_get_credentials_missing() {
    let config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
    let tunnel = HttpProxyTunnel::new(config);
    assert!(tunnel.get_credentials().is_err());
}

#[test]
fn test_tunnel_get_credentials_username_only() {
    let mut config = HttpProxyConfig::new("p".into(), 3128, "t".into(), 443);
    config.proxy_username = Some("user".into());
    let tunnel = HttpProxyTunnel::new(config);
    let (u, p) = tunnel.get_credentials().unwrap();
    assert_eq!(u, "user");
    assert_eq!(p, "");
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
    assert!(req.contains("User-Agent: aria2-rust/1.0\r\n"));
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
    let challenge =
        DigestAuthChallenge::parse(r#"Digest realm="Proxy", nonce="abc123", qop="auth""#).unwrap();

    let auth = proxy_digest_auth("user", "pass", "CONNECT", "target.com:443", &challenge, 1);
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

#[test]
fn test_tunnel_connect_request_format_complete() {
    let config = HttpProxyConfig::new("proxy.local".into(), 8080, "github.com".into(), 443);
    let tunnel = HttpProxyTunnel::new(config);
    let req = tunnel.build_connect_request(None);

    // Verify exact format
    let expected = "CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Connection: keep-alive\r\n\r\n";
    assert_eq!(req, expected);
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
