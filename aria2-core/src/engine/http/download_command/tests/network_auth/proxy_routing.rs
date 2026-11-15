use super::*;

#[tokio::test]
async fn all_proxy_percent_decodes_embedded_basic_credentials() {
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP proxy fixture");
    let proxy_address = listener.local_addr().expect("read proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept proxy connection");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let read = stream.read(&mut chunk).await.expect("read proxy request");
            assert_ne!(read, 0, "proxy request ended before headers");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let expected = format!(
            "Proxy-Authorization: Basic {}",
            base64::engine::general_purpose::STANDARD.encode("user@name:p:@%")
        );
        let request = String::from_utf8_lossy(&request);
        assert!(
            request
                .lines()
                .any(|line| line.eq_ignore_ascii_case(&expected)),
            "proxy received unexpected authorization header: {request}"
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write proxy response");
    });

    let uri = "http://download.example.test/file.bin";
    let options = DownloadOptions {
        all_proxy: Some(format!(
            "http://user%40name:p%3A%40%25@proxy.invalid:{}",
            proxy_address.port()
        )),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1210),
        vec![uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_network_addresses(
        group,
        uri,
        &options,
        None,
        None,
        crate::engine::http::client_config::ResolvedNetworkAddresses {
            target: None,
            proxy: Some(vec![proxy_address]),
        },
        Arc::new(OutboundNetworkPolicy::direct()),
    )
    .expect("build HTTP-proxied download");

    let response = command
        .client
        .get(uri)
        .send()
        .await
        .expect("proxy should accept the request");
    assert_eq!(response.text().await.expect("read response body"), "ok");
    server.await.expect("proxy fixture should finish");
}

#[tokio::test]
async fn all_proxy_path_at_sign_does_not_become_proxy_credentials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind HTTP proxy fixture");
    let proxy_address = listener.local_addr().expect("read proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept proxy connection");
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let read = stream.read(&mut chunk).await.expect("read proxy request");
            assert_ne!(read, 0, "proxy request ended before headers");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&request);
        assert!(
            !request.lines().any(|line| line
                .to_ascii_lowercase()
                .starts_with("proxy-authorization:")),
            "path/query @ characters must not synthesize proxy credentials: {request}"
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write proxy response");
    });

    let uri = "http://download.example.test/file.bin";
    let options = DownloadOptions {
        all_proxy: Some(format!(
            "http://proxy.invalid:{}/path@marker?next=@query",
            proxy_address.port()
        )),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1211),
        vec![uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_network_addresses(
        group,
        uri,
        &options,
        None,
        None,
        crate::engine::http::client_config::ResolvedNetworkAddresses {
            target: None,
            proxy: Some(vec![proxy_address]),
        },
        Arc::new(OutboundNetworkPolicy::direct()),
    )
    .expect("build HTTP-proxied download");

    let response = command
        .client
        .get(uri)
        .send()
        .await
        .expect("proxy URL suffix should not prevent routing");
    assert_eq!(response.text().await.expect("read response body"), "ok");
    server.await.expect("proxy fixture should finish");
}

#[tokio::test]
async fn all_proxy_socks5_routes_http_and_authenticates() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SOCKS5 proxy fixture");
    let proxy_address = listener.local_addr().expect("read SOCKS5 proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept SOCKS5 connection");

        let mut greeting = [0; 3];
        stream
            .read_exact(&mut greeting)
            .await
            .expect("read SOCKS5 methods");
        assert_eq!(greeting, [5, 1, 2]);
        stream
            .write_all(&[5, 2])
            .await
            .expect("select username/password auth");

        let mut auth_header = [0; 2];
        stream
            .read_exact(&mut auth_header)
            .await
            .expect("read SOCKS5 auth header");
        assert_eq!(auth_header[0], 1);
        let mut username = vec![0; usize::from(auth_header[1])];
        stream
            .read_exact(&mut username)
            .await
            .expect("read SOCKS5 username");
        let mut password_length = [0; 1];
        stream
            .read_exact(&mut password_length)
            .await
            .expect("read SOCKS5 password length");
        let mut password = vec![0; usize::from(password_length[0])];
        stream
            .read_exact(&mut password)
            .await
            .expect("read SOCKS5 password");
        assert_eq!(username, b"proxy-user");
        assert_eq!(password, b"proxy-pass");
        stream
            .write_all(&[1, 0])
            .await
            .expect("accept SOCKS5 credentials");

        let mut request = [0; 4];
        stream
            .read_exact(&mut request)
            .await
            .expect("read SOCKS5 CONNECT request");
        assert_eq!(request, [5, 1, 0, 1]);
        let mut target = [0; 6];
        stream
            .read_exact(&mut target)
            .await
            .expect("read SOCKS5 target address");
        assert_eq!(&target[..4], &[127, 0, 0, 1]);
        assert_eq!(u16::from_be_bytes([target[4], target[5]]), 8080);
        stream
            .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
            .await
            .expect("establish SOCKS5 tunnel");

        let mut request = [0; 1024];
        let length = stream
            .read(&mut request)
            .await
            .expect("read tunneled HTTP request");
        assert!(
            String::from_utf8_lossy(&request[..length]).starts_with("GET /file.bin HTTP/1.1\r\n")
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("respond through SOCKS5 tunnel");
    });

    let uri = "http://127.0.0.1:8080/file.bin";
    let options = DownloadOptions {
        all_proxy: Some(format!(
            "socks5://proxy-user:proxy-pass@proxy.invalid:{}",
            proxy_address.port()
        )),
        no_proxy: Some("not-target.invalid".to_string()),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1202),
        vec![uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_network_addresses(
        group,
        uri,
        &options,
        None,
        None,
        crate::engine::http::client_config::ResolvedNetworkAddresses {
            target: None,
            proxy: Some(vec![proxy_address]),
        },
        Arc::new(OutboundNetworkPolicy::direct()),
    )
    .expect("build SOCKS5-proxied download with resolved proxy address");

    let response = command
        .client
        .get(uri)
        .send()
        .await
        .expect("HTTP request should use the SOCKS5 proxy");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.expect("read response body"), "ok");
    server.await.expect("SOCKS5 proxy fixture should finish");
}

#[tokio::test]
async fn all_proxy_socks4_routes_http() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind SOCKS4 proxy fixture");
    let proxy_address = listener.local_addr().expect("read SOCKS4 proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept SOCKS4 connection");
        let mut request = [0; 9];
        stream
            .read_exact(&mut request)
            .await
            .expect("read SOCKS4 CONNECT request");
        assert_eq!(request, [4, 1, 0x1f, 0x90, 127, 0, 0, 1, 0]);
        stream
            .write_all(&[0, 90, 0, 0, 0, 0, 0, 0])
            .await
            .expect("establish SOCKS4 tunnel");

        let mut request = [0; 1024];
        let length = stream
            .read(&mut request)
            .await
            .expect("read tunneled HTTP request");
        assert!(
            String::from_utf8_lossy(&request[..length]).starts_with("GET /file.bin HTTP/1.1\r\n")
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("respond through SOCKS4 tunnel");
    });

    let uri = "http://127.0.0.1:8080/file.bin";
    let options = DownloadOptions {
        all_proxy: Some(format!("socks4://proxy.invalid:{}", proxy_address.port())),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1203),
        vec![uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_network_addresses(
        group,
        uri,
        &options,
        None,
        None,
        crate::engine::http::client_config::ResolvedNetworkAddresses {
            target: None,
            proxy: Some(vec![proxy_address]),
        },
        Arc::new(OutboundNetworkPolicy::direct()),
    )
    .expect("build SOCKS4-proxied download with resolved proxy address");

    let response = command
        .client
        .get(uri)
        .send()
        .await
        .expect("HTTP request should use the SOCKS4 proxy");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(response.text().await.expect("read response body"), "ok");
    server.await.expect("SOCKS4 proxy fixture should finish");
}

#[tokio::test]
async fn proxy_client_uses_the_resolved_proxy_addresses() {
    use crate::engine::http::client_config::ResolvedNetworkAddresses;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind resolved-proxy fixture");
    let proxy_address = listener
        .local_addr()
        .expect("read resolved-proxy fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept connection through resolved proxy address");
        let mut request = [0u8; 4096];
        let length = stream
            .read(&mut request)
            .await
            .expect("read request sent through resolved proxy");
        let request = String::from_utf8_lossy(&request[..length]);
        assert!(request.starts_with("GET http://origin.invalid/file.bin HTTP/1.1\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("respond through resolved proxy");
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://proxy.invalid:{}", proxy_address.port())),
        ..DownloadOptions::default()
    };
    let uri = "http://origin.invalid/file.bin";
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1200),
        vec![uri.to_string()],
        options.clone(),
    )));
    let command = DownloadCommand::new_with_group_and_resolved_network_addresses(
        group,
        uri,
        &options,
        None,
        None,
        ResolvedNetworkAddresses {
            target: None,
            proxy: Some(vec![proxy_address]),
        },
        Arc::new(OutboundNetworkPolicy::direct()),
    )
    .expect("build proxy client with supplied resolver addresses");

    let response = command
        .client
        .get(uri)
        .send()
        .await
        .expect("client must connect to the resolved proxy address");
    assert_eq!(response.status().as_u16(), 200);
    server.await.expect("resolved-proxy fixture must finish");
}

#[tokio::test]
async fn proxy_client_leaves_redirects_for_the_download_flow() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind local proxy fixture");
    let proxy_addr = listener.local_addr().expect("read proxy address");
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests_for_server = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        for request_number in 1..=2 {
            let accepted = if request_number == 1 {
                Some(
                    listener
                        .accept()
                        .await
                        .expect("accept initial proxy request"),
                )
            } else {
                tokio::time::timeout(std::time::Duration::from_millis(250), listener.accept())
                    .await
                    .ok()
                    .map(|result| result.expect("accept redirected proxy request"))
            };
            let Some((mut stream, _)) = accepted else {
                break;
            };
            requests_for_server.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut request = vec![0; 4096];
            let bytes = stream.read(&mut request).await.expect("read proxy request");
            assert!(bytes > 0, "proxy request should not be empty");
            let response = if request_number == 1 {
                b"HTTP/1.1 302 Found\r\nLocation: http://origin.example/redirect-target\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
            };
            stream
                .write_all(response)
                .await
                .expect("write proxy response");
        }
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let command = DownloadCommand::new(
        GroupId::new(12),
        "http://origin.example/file.bin",
        &options,
        None,
        None,
    )
    .expect("create proxied download command");

    let response = command
        .client
        .get("http://origin.example/file.bin")
        .send()
        .await
        .expect("proxy should return the redirect response");
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "proxy redirects must be handled by SequentialDownloader so URI and retry state stay canonical"
    );

    server.await.expect("proxy fixture should finish");
}

#[tokio::test]
async fn proxied_client_binds_source_for_the_proxy_peer() {
    use std::net::{IpAddr, Ipv4Addr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("bind source-aware proxy fixture");
    let proxy_addr = listener
        .local_addr()
        .expect("read source-aware proxy address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener
            .accept()
            .await
            .expect("accept source-aware proxy request");
        let mut request = [0u8; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read source-aware proxy request");
        assert!(bytes > 0, "proxy request should not be empty");
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await
            .expect("write source-aware proxy response");
        peer
    });

    let options = DownloadOptions {
        http_proxy: Some(format!("http://{proxy_addr}")),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1201),
        vec!["http://[::1]:9/file.bin".to_string()],
        options.clone(),
    )));
    let policy = Arc::new(
        OutboundNetworkPolicy::new(vec![
            IpAddr::V6("::1".parse().expect("parse IPv6 source")),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)),
        ])
        .expect("source policy should accept both address families"),
    );
    let command = DownloadCommand::new_with_group_and_resolved_addresses_and_policy(
        group,
        "http://[::1]:9/file.bin",
        &options,
        None,
        None,
        None,
        policy,
    )
    .expect("proxy client must select a source compatible with the proxy peer");

    let response = command
        .client
        .get("http://[::1]:9/file.bin")
        .send()
        .await
        .expect("proxy should serve the IPv6-target request");
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        server.await.expect("proxy fixture should finish").ip(),
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2))
    );
}
