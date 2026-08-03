use super::*;
use std::io::Cursor;
use std::net::{Ipv4Addr, SocketAddrV4};

#[derive(Debug)]
struct MockTcpStream {
    reader: Cursor<Vec<u8>>,
    writer: Vec<u8>,
}

impl MockTcpStream {
    fn new(read_data: Vec<u8>) -> Self {
        Self {
            reader: Cursor::new(read_data),
            writer: Vec::new(),
        }
    }

    fn into_write(self) -> Vec<u8> {
        self.writer
    }
}

impl std::io::Read for MockTcpStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl std::io::Write for MockTcpStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.writer.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn test_socks4_happy_path() {
    let connector = Socks4Connector::new("testuser");
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 8080).into();

    let mock_response: Vec<u8> = vec![0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_ok(), "SOCKS4 happy path should succeed");

    let written = result.unwrap().into_write();
    assert_eq!(written[0], 0x04, "version byte should be 0x04");
    assert_eq!(written[1], 0x01, "command byte should be 0x01 (connect)");
    assert_eq!(&written[4..8], &[127, 0, 0, 1], "IP should be 127.0.0.1");
    assert_eq!(
        &written[8..17],
        b"testuser\0",
        "user ID should be null-terminated"
    );
}

#[test]
fn test_socks4_rejected_error() {
    let connector = Socks4Connector::new("user");
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234).into();

    let mock_response: Vec<u8> = vec![0x00, 0x91, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err(), "SOCKS4 rejection should return error");
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("rejected"),
        "error message should mention rejection: {}",
        err_msg
    );
}

#[test]
fn test_socks4_identd_error() {
    let connector = Socks4Connector::new("user");
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234).into();

    let mock_response: Vec<u8> = vec![0x00, 0x92, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("identd"),
        "error message should mention identd: {}",
        err_msg
    );
}

#[test]
fn test_socks4_userid_mismatch_error() {
    let connector = Socks4Connector::new("user");
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 1234).into();

    let mock_response: Vec<u8> = vec![0x00, 0x93, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err());
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("identd") && err_msg.contains("different"),
        "error message should mention identd/user-id mismatch: {}",
        err_msg
    );
}

#[test]
fn test_socks4_empty_user_id() {
    let connector = Socks4Connector::new("");
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 1), 443).into();

    let mock_response: Vec<u8> = vec![0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_ok(), "empty user ID should work");
    let written = result.unwrap().into_write();
    assert_eq!(
        &written[4..9],
        &[192, 168, 1, 1, 0x00],
        "null terminator after IP when no user ID"
    );
}

#[test]
fn test_socks5_no_auth_happy_path() {
    let connector = Socks5Connector::no_auth();
    let target: std::net::SocketAddr =
        SocketAddrV4::new(Ipv4Addr::new(93, 184, 216, 34), 443).into();

    let mock_response: Vec<u8> = vec![
        0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x90,
    ];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_ok(), "SOCKS5 no-auth happy path should succeed");

    let written = result.unwrap().into_write();
    assert_eq!(written[0], 0x05, "greeting version should be 0x05");
    assert_eq!(written[1], 0x01, "should offer exactly 1 method (no-auth)");
    assert_eq!(written[2], 0x00, "method offered should be 0x00 (no-auth)");
    let conn_offset = 3;
    assert_eq!(written[conn_offset], 0x05, "connect request version");
    assert_eq!(written[conn_offset + 1], 0x01, "connect command");
    assert_eq!(written[conn_offset + 2], 0x00, "reserved");
    assert_eq!(written[conn_offset + 3], 0x01, "ATYP IPv4");
    assert_eq!(
        &written[conn_offset + 4..conn_offset + 8],
        &[93, 184, 216, 34],
        "target IP"
    );
    assert_eq!(
        &written[conn_offset + 8..conn_offset + 10],
        &0x01BBu16.to_be_bytes(),
        "target port 443"
    );
}

#[test]
fn test_socks5_username_password_auth_happy_path() {
    let connector = Socks5Connector::new(Some("myuser".to_string()), Some("mypass".to_string()));
    let target: std::net::SocketAddr =
        SocketAddrV4::new(Ipv4Addr::new(10, 20, 30, 40), 8080).into();

    let mock_response: Vec<u8> = vec![
        0x05, 0x02, 0x01, 0x00, 0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x90,
    ];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(
        result.is_ok(),
        "SOCKS5 username/password auth should succeed"
    );

    let written = result.unwrap().into_write();
    assert_eq!(written[0], 0x05, "greeting version");
    assert_eq!(written[1], 0x02, "should offer 2 methods");
    assert_eq!(written[2], 0x00, "no-auth method");
    assert_eq!(written[3], 0x02, "username/password method");
    let auth_offset = 4;
    assert_eq!(written[auth_offset], 0x01, "auth sub-negotiation version");
    assert_eq!(written[auth_offset + 1], 6, "username length");
    assert_eq!(
        &written[auth_offset + 2..auth_offset + 8],
        b"myuser",
        "username"
    );
    assert_eq!(written[auth_offset + 8], 6, "password length");
    assert_eq!(
        &written[auth_offset + 9..auth_offset + 15],
        b"mypass",
        "password"
    );
}

#[test]
fn test_socks5_auth_failure() {
    let connector = Socks5Connector::new(Some("bad".to_string()), Some("cred".to_string()));
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 5678).into();

    let mock_response: Vec<u8> = vec![0x05, 0x02, 0x01, 0x01];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err(), "auth failure should be an error");
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("authentication failed") || err_msg.contains("failed"),
        "error should mention auth failure: {}",
        err_msg
    );
}

#[test]
fn test_socks5_connection_refused() {
    let connector = Socks5Connector::no_auth();
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(8, 8, 8, 8), 53).into();

    let mock_response: Vec<u8> = vec![
        0x05, 0x00, 0x05, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err(), "connection refused should be an error");
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("refused"),
        "error should mention connection refused: {}",
        err_msg
    );
}

#[test]
fn test_socks5_unacceptable_method() {
    let connector = Socks5Connector::no_auth();
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, 1), 80).into();

    let mock_response: Vec<u8> = vec![0x05, 0xFF];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err(), "unacceptable method should be an error");
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("unacceptable") || err_msg.contains("0xFF"),
        "error should mention unacceptable method: {}",
        err_msg
    );
}

#[test]
fn test_socks5_general_failure() {
    let connector = Socks5Connector::no_auth();
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(0, 0, 0, 0), 1).into();

    let mock_response: Vec<u8> = vec![
        0x05, 0x00, 0x05, 0x01, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err(), "general failure should be an error");
    let err_msg = result.err().unwrap();
    assert!(
        err_msg.contains("general") || err_msg.contains("failure"),
        "error should mention general failure: {}",
        err_msg
    );
}

// ==================== E7: New Proxy Tests ====================

// Test 1: SOCKS4 connect success (valid response bytes -> Ok)
#[test]
fn e7_test_socks4_connect_success() {
    let connector = Socks4Connector::new("proxyuser");
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(172, 16, 0, 1), 443).into();

    let mock_response: Vec<u8> = vec![0x00, 0x5a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(
        result.is_ok(),
        "SOCKS4 connect with valid response should succeed"
    );

    let written = result.unwrap().into_write();
    assert_eq!(written[0], 0x04, "SOCKS version byte");
    assert_eq!(written[1], 0x01, "CONNECT command");
}

// Test 2: SOCKS4 connect fail (error code 0x91 -> Err)
#[test]
fn e7_test_socks4_connect_fail_rejected() {
    let connector = Socks4Connector::new("test");
    let target: std::net::SocketAddr =
        SocketAddrV4::new(Ipv4Addr::new(192, 168, 1, 100), 80).into();

    let mock_response: Vec<u8> = vec![0x00, 0x91, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_err(), "SOCKS4 error 0x91 should return Err");
    assert!(
        result.unwrap_err().contains("rejected"),
        "error message must contain 'rejected'"
    );
}

// Test 3: SOCKS5 no-auth connect (Greeting 0x00 + Connect success 0x00)
#[test]
fn e7_test_socks5_no_auth_connect() {
    let connector = Socks5Connector::no_auth();
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(10, 0, 0, 1), 8080).into();

    let mock_response: Vec<u8> = vec![
        0x05, 0x00, 0x05, 0x00, 0x00, 0x01, 0x0a, 0x00, 0x00, 0x01, 0x1f, 0x90,
    ];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(result.is_ok(), "SOCKS5 no-auth connect should succeed");

    let written = result.unwrap().into_write();
    assert_eq!(written[0], 0x05, "greeting version");
    assert_eq!(written[1], 0x01, "one method offered");
    assert_eq!(written[2], 0x00, "no-auth method");
}

// Test 4: SOCKS5 password auth (Greeting 0x02 + Auth success + Connect success)
#[test]
fn e7_test_socks5_password_auth_connect() {
    let connector = Socks5Connector::new(Some("admin".to_string()), Some("secret123".to_string()));
    let target: std::net::SocketAddr = SocketAddrV4::new(Ipv4Addr::new(1, 2, 3, 4), 9090).into();

    let mock_response: Vec<u8> = vec![
        0x05, 0x02, 0x01, 0x00, 0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x23, 0x82,
    ];
    let mock_stream = MockTcpStream::new(mock_response);

    let result = connector.connect(mock_stream, &target);
    assert!(
        result.is_ok(),
        "SOCKS5 password auth connect should succeed"
    );

    let written = result.unwrap().into_write();
    assert_eq!(written[0], 0x05, "greeting version");
    assert_eq!(written[1], 0x02, "two methods offered");
    assert_eq!(written[3], 0x02, "username/password method offered");

    let auth_offset = 4;
    assert_eq!(written[auth_offset], 0x01, "auth sub-version");
    assert_eq!(written[auth_offset + 1], 5, "username length 'admin'");
    assert_eq!(
        &written[auth_offset + 2..auth_offset + 7],
        b"admin",
        "username bytes"
    );
    assert_eq!(written[auth_offset + 7], 9, "password length 'secret123'");
    assert_eq!(
        &written[auth_offset + 8..auth_offset + 17],
        b"secret123",
        "password bytes"
    );
}

// Test 5: No-proxy bypass matcher
#[test]
fn e7_test_no_proxy_bypass_matcher() {
    let matcher = NoProxyMatcher::from_env_value("*.local,localhost,example.com,.internal");

    // Should bypass: wildcard *.local matches api.local
    assert!(
        matcher.should_bypass_hostname("api.local"),
        "*.local should match api.local"
    );

    // Should NOT bypass: example.com is not in the list (example.org is different)
    assert!(
        !matcher.should_bypass_hostname("example.org"),
        "example.org should not bypass"
    );

    // Should bypass: exact match localhost
    assert!(
        matcher.should_bypass_hostname("localhost"),
        "exact match localhost should bypass"
    );

    // Should bypass: exact match example.com
    assert!(
        matcher.should_bypass_hostname("example.com"),
        "exact match example.com should bypass"
    );

    // Should bypass: .internal wildcard matches sub.internal
    assert!(
        matcher.should_bypass_hostname("sub.internal"),
        ".internal should match sub.internal"
    );

    // Should bypass: .internal matches internal itself
    assert!(
        matcher.should_bypass_hostname("internal"),
        ".internal should also match bare domain"
    );

    // Should NOT bypass: random external host
    assert!(
        !matcher.should_bypass_hostname("google.com"),
        "google.com should not bypass"
    );
}

// Test 5b: No-proxy IP-based matching
#[test]
fn e7_test_no_proxy_ip_matching() {
    use std::net::{IpAddr, Ipv4Addr};

    let matcher = NoProxyMatcher::from_env_value("192.168.1.1,10.0.0.0/8");

    let addr_v4_192: std::net::SocketAddr =
        std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 80);
    assert!(
        matcher.should_bypass(&addr_v4_192),
        "exact IP 192.168.1.1 should bypass"
    );

    let addr_v4_10: std::net::SocketAddr =
        std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 5, 3, 1)), 8080);
    assert!(
        matcher.should_bypass(&addr_v4_10),
        "10.5.3.1 should be within 10.0.0.0/8"
    );

    let addr_v4_external: std::net::SocketAddr =
        std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)), 443);
    assert!(
        !matcher.should_bypass(&addr_v4_external),
        "172.16.0.1 should NOT be within 10.0.0.0/8"
    );
}

// Test 5c: No-proxy wildcard * matches everything
#[test]
fn e7_test_no_proxy_wildcard_all() {
    let matcher = NoProxyMatcher::from_env_value("*");

    assert!(
        matcher.should_bypass_hostname("anything"),
        "* should bypass any hostname"
    );
    assert!(
        matcher.should_bypass(&"127.0.0.1:80".parse::<std::net::SocketAddr>().unwrap()),
        "* should bypass any IP address"
    );
}

// Test 6: Proxy URL parsing
#[test]
fn e7_test_proxy_url_parsing_socks5_with_credentials() {
    let url = "socks5://user:pass@127.0.0.1:1080";
    let parsed = ProxyUrl::parse(url).expect("should parse socks5 URL");

    assert_eq!(parsed.protocol, ProxyProtocol::Socks5);
    assert_eq!(parsed.host, "127.0.0.1");
    assert_eq!(parsed.port, 1080);
    assert_eq!(parsed.username, Some("user".to_string()));
    assert_eq!(parsed.password, Some("pass".to_string()));
}

// Test 6b: Parse socks4 URL without credentials
#[test]
fn e7_test_proxy_url_parsing_socks4_no_credentials() {
    let parsed =
        ProxyUrl::parse("socks4://proxy.example.com:1080").expect("should parse socks4 URL");

    assert_eq!(parsed.protocol, ProxyProtocol::Socks4);
    assert_eq!(parsed.host, "proxy.example.com");
    assert_eq!(parsed.port, 1080);
    assert!(parsed.username.is_none());
    assert!(parsed.password.is_none());
}

// Test 6c: Parse HTTP proxy URL
#[test]
fn e7_test_proxy_url_parsing_http() {
    let parsed = ProxyUrl::parse("http://admin:secret@proxy.corp.com:3128")
        .expect("should parse http proxy URL");

    assert_eq!(parsed.protocol, ProxyProtocol::Http);
    assert_eq!(parsed.host, "proxy.corp.com");
    assert_eq!(parsed.port, 3128);
    assert_eq!(parsed.username, Some("admin".to_string()));
    assert_eq!(parsed.password, Some("secret".to_string()));
}

// Test 6d: Default port when omitted
#[test]
fn e7_test_proxy_url_default_port() {
    let socks5_parsed = ProxyUrl::parse("socks5://proxy.local").expect("should parse");
    assert_eq!(socks5_parsed.port, 1080, "SOCKS default port is 1080");

    let http_parsed = ProxyUrl::parse("http://webproxy.local").expect("should parse");
    assert_eq!(http_parsed.port, 8080, "HTTP default port is 8080");

    let https_parsed = ProxyUrl::parse("https://secure.local").expect("should parse");
    assert_eq!(https_parsed.port, 443, "HTTPS default port is 443");
}

// Test 6e: Invalid protocol returns error
#[test]
fn e7_test_proxy_url_invalid_protocol() {
    let result = ProxyUrl::parse("ftp://host:21");
    assert!(result.is_err(), "unsupported protocol should return error");
    assert!(
        result.unwrap_err().contains("Unsupported"),
        "error should mention unsupported protocol"
    );
}

// Test 6f: Create connector from parsed URL
#[test]
fn e7_test_create_connector_from_url() {
    let url = ProxyUrl::parse("socks5://myuser:mypass@10.0.0.1:9050").expect("should parse");
    let _connector = url.create_connector();
}
