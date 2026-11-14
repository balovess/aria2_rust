use super::*;

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

#[tokio::test]
async fn authentication_retry_follows_redirect_and_preserves_protection_space() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth redirect fixture");
    let address = listener.local_addr().expect("read auth redirect address");
    let server = tokio::spawn(async move {
        for request_number in 1..=3 {
            let (mut stream, _) = listener.accept().await.expect("accept auth request");
            let mut request = vec![0; 4096];
            let bytes = stream.read(&mut request).await.expect("read auth request");
            let request = String::from_utf8_lossy(&request[..bytes]);
            let has_authorization = request.lines().any(|line| {
                line.to_ascii_lowercase()
                    .starts_with("authorization: basic ")
            });

            match request_number {
                1 => {
                    assert!(request.starts_with("GET /protected/file.bin HTTP/1.1\r\n"));
                    assert!(
                        !has_authorization,
                        "initial request must be unauthenticated"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth challenge");
                }
                2 => {
                    assert!(request.starts_with("GET /protected/file.bin HTTP/1.1\r\n"));
                    assert!(has_authorization, "auth retry must include credentials");
                    stream
                        .write_all(
                            b"HTTP/1.1 302 Found\r\nLocation: /protected/final.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write auth redirect");
                }
                3 => {
                    assert!(request.starts_with("GET /protected/final.bin HTTP/1.1\r\n"));
                    assert!(
                        has_authorization,
                        "same-host redirect must preserve the activated protection space"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nContent-Disposition: attachment; filename=authenticated.bin\r\nConnection: close\r\n\r\nauth-redirect\n",
                        )
                        .await
                        .expect("write final authenticated response");
                }
                _ => unreachable!(),
            }
        }
    });

    let directory = tempfile::tempdir().expect("create auth redirect directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("password".to_string()),
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/protected/file.bin");
    let output = directory.path().join("authenticated.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(15),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
    )
    .expect("create auth redirect command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("auth redirect download should not hang")
        .expect("auth retry redirect should complete");
    assert_eq!(
        std::fs::read(&output).expect("read authenticated redirect output"),
        b"auth-redirect\n"
    );
    server.await.expect("auth redirect fixture should finish");
}

#[tokio::test]
async fn metadata_get_challenge_is_reused_by_the_range_probe() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind metadata auth fixture");
    let address = listener.local_addr().expect("metadata auth address");
    let server = tokio::spawn(async move {
        let (mut unauthenticated, _) = listener.accept().await.expect("accept metadata GET");
        let request = read_http_request(&mut unauthenticated).await;
        assert!(request.starts_with("GET /metadata.bin HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        unauthenticated
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write metadata auth challenge");

        let (mut authenticated, _) = listener.accept().await.expect("accept metadata retry");
        let request = read_http_request(&mut authenticated).await;
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        authenticated
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated metadata response");

        let (mut range, _) = listener.accept().await.expect("accept Range probe");
        let request = read_http_request(&mut range).await;
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("range") && value.trim() == "bytes=0-0"
            })
        }));
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        range
            .write_all(
                b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/10\r\nContent-Length: 1\r\nConnection: close\r\n\r\n0",
            )
            .await
            .expect("write authenticated Range response");
    });

    let directory = tempfile::tempdir().expect("create metadata auth directory");
    let options = DownloadOptions {
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        http_auth_challenge: true,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/metadata.bin");
    let command = DownloadCommand::new(
        GroupId::new(152),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("metadata.bin"),
    )
    .expect("create metadata auth command");
    let prepared = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        command.send_get_with_redirects(&uri),
    )
    .await
    .expect("metadata GET should not hang")
    .expect("metadata GET should return a response");

    let auth_options = crate::http::AuthResolveOptions {
        http_auth_challenge: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        ..crate::http::AuthResolveOptions::default()
    };
    let prober =
        crate::engine::http::segment_downloader::HttpSegmentDownloader::new(&command.client)
            .with_shared_auth_options(auth_options, prepared.auth_factory);
    let probe = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        prober.probe_range_metadata(&uri, None),
    )
    .await
    .expect("Range probe should not hang")
    .expect("Range probe should reuse the activated metadata credentials");

    assert!(probe.supports_range);
    server.await.expect("metadata auth fixture should finish");
}

#[tokio::test]
async fn metadata_get_challenge_is_reused_when_sequential_download_reissues_get() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sequential metadata auth fixture");
    let address = listener
        .local_addr()
        .expect("sequential metadata auth address");
    let server = tokio::spawn(async move {
        let (mut unauthenticated, _) = listener.accept().await.expect("accept metadata GET");
        let mut request = vec![0; 4096];
        let bytes = unauthenticated
            .read(&mut request)
            .await
            .expect("read metadata GET");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /sequential.bin HTTP/1.1\r\n"));
        assert!(!request.to_ascii_lowercase().contains("authorization:"));
        unauthenticated
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"download\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write metadata auth challenge");

        let (mut authenticated, _) = listener.accept().await.expect("accept metadata retry");
        let mut request = vec![0; 4096];
        let bytes = authenticated
            .read(&mut request)
            .await
            .expect("read metadata auth retry");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        authenticated
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write authenticated metadata response");

        let (mut sequential, _) = listener.accept().await.expect("accept sequential GET");
        let mut request = vec![0; 4096];
        let bytes = sequential
            .read(&mut request)
            .await
            .expect("read sequential GET");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /sequential.bin HTTP/1.1\r\n"));
        assert!(request.lines().any(|line| {
            line.split_once(':').is_some_and(|(name, value)| {
                name.eq_ignore_ascii_case("authorization") && value.trim() == "Basic dXNlcjpwYXNz"
            })
        }));
        sequential
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
            )
            .await
            .expect("write sequential response");
    });

    let directory = tempfile::tempdir().expect("create sequential auth directory");
    let output = directory.path().join("sequential.bin");
    let options = DownloadOptions {
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        http_auth_challenge: true,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/sequential.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(153),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
    )
    .expect("create sequential metadata auth command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("sequential metadata auth download should not hang")
        .expect("sequential metadata auth download should complete");
    assert_eq!(
        std::fs::read(&output).expect("read sequential auth output"),
        b"0123456789"
    );
    server.await.expect("sequential auth fixture should finish");
}

#[tokio::test]
async fn sequential_transfer_failure_retry_reuses_activated_basic_credentials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind sequential retry auth fixture");
    let address = listener
        .local_addr()
        .expect("read sequential retry auth address");
    let server = tokio::spawn(async move {
        for request_number in 1..=4 {
            let (mut stream, _) = listener
                .accept()
                .await
                .expect("accept sequential retry request");
            let mut request = Vec::new();
            let mut chunk = [0_u8; 1024];
            loop {
                let count = stream
                    .read(&mut chunk)
                    .await
                    .expect("read sequential retry request");
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("request headers should be UTF-8");
            assert!(request.starts_with("GET /retry.bin HTTP/1.1\r\n"));
            let has_basic_auth = request.lines().any(|line| {
                line.split_once(':').is_some_and(|(name, value)| {
                    name.eq_ignore_ascii_case("authorization")
                        && value.trim() == "Basic dXNlcjpwYXNz"
                })
            });

            match request_number {
                1 => {
                    assert!(
                        !has_basic_auth,
                        "first request should challenge credentials"
                    );
                    stream
                        .write_all(
                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"retry\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await
                        .expect("write Basic challenge");
                }
                2 => {
                    assert!(has_basic_auth, "metadata auth retry must be authenticated");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                        )
                        .await
                        .expect("write authenticated metadata response");
                }
                3 => {
                    assert!(has_basic_auth, "first transfer request must retain auth");
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123",
                        )
                        .await
                        .expect("write intentionally truncated transfer response");
                }
                4 => {
                    assert!(
                        has_basic_auth,
                        "transfer-failure retry must reuse the activated credentials: {request}"
                    );
                    let range_start = request.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        let range = if name.eq_ignore_ascii_case("range") {
                            value.trim().strip_prefix("bytes=")
                        } else {
                            None
                        }?;
                        range
                            .split_once('-')
                            .and_then(|(start, _)| start.parse::<usize>().ok())
                    });
                    if let Some(start) = range_start.filter(|start| *start < 10) {
                        let body = &b"0123456789"[start..];
                        stream
                            .write_all(
                                format!(
                                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-9/10\r\nConnection: close\r\n\r\n",
                                    body.len(),
                                    start
                                )
                                .as_bytes(),
                            )
                            .await
                            .expect("write retry range response headers");
                        stream
                            .write_all(body)
                            .await
                            .expect("write retry range response body");
                    } else {
                        stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n0123456789",
                            )
                            .await
                            .expect("write full retry response");
                    }
                }
                _ => unreachable!(),
            }
        }
    });

    let directory = tempfile::tempdir().expect("create sequential retry output directory");
    let output = directory.path().join("retry.bin");
    let options = DownloadOptions {
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        force_sequential: true,
        http_user: Some("user".to_string()),
        http_passwd: Some("pass".to_string()),
        http_auth_challenge: true,
        max_retries: 2,
        retry_wait: 0,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/retry.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(154),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        None,
    )
    .expect("create sequential retry auth command");

    tokio::time::timeout(std::time::Duration::from_secs(8), command.execute())
        .await
        .expect("authenticated sequential retry should not hang")
        .expect("authenticated sequential retry should complete");
    assert_eq!(
        std::fs::read(&output).expect("read retried output"),
        b"0123456789"
    );
    server
        .await
        .expect("sequential retry auth fixture should finish");
}

#[tokio::test]
async fn cross_origin_redirect_drops_origin_credentials_and_sensitive_custom_headers() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let target_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect target fixture");
    let target_address = target_listener
        .local_addr()
        .expect("read redirect target address");
    let target = tokio::spawn(async move {
        let (mut stream, _) = target_listener
            .accept()
            .await
            .expect("accept redirected request");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read redirected request");
        let request = String::from_utf8_lossy(&request[..bytes]).to_ascii_lowercase();
        assert!(request.starts_with("get /target.bin http/1.1\r\n"));
        assert!(!request.contains("authorization:"));
        assert!(!request.contains("x-api-key:"));
        assert!(!request.contains("x-session-token:"));
        assert!(request.contains("x-trace-id: trace-123\r\n"));
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n")
            .await
            .expect("write redirect target response");
    });

    let source_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind redirect source fixture");
    let source_address = source_listener
        .local_addr()
        .expect("read redirect source address");
    let source = tokio::spawn(async move {
        for request_number in 1..=2 {
            let (mut stream, _) = source_listener
                .accept()
                .await
                .expect("accept source request");
            let mut request = vec![0; 4096];
            let bytes = stream
                .read(&mut request)
                .await
                .expect("read source request");
            let request = String::from_utf8_lossy(&request[..bytes]).to_ascii_lowercase();
            assert!(request.contains("x-api-key: api-secret\r\n"));
            assert!(request.contains("x-session-token: session-secret\r\n"));
            if request_number == 1 {
                assert!(!request.contains("authorization:"));
                stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"origin\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .expect("write origin auth challenge");
            } else {
                assert!(
                    request.contains("authorization: basic dxnlcjpwyxnzd29yza==\r\n"),
                    "source auth retry should include configured credentials: {request}"
                );
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/target.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .expect("write cross-origin redirect");
            }
        }
    });

    let directory = tempfile::tempdir().expect("create redirect output directory");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        http_user: Some("user".to_string()),
        http_passwd: Some("password".to_string()),
        http_auth_challenge: true,
        header: vec![
            "X-API-Key: api-secret".to_string(),
            "X-Session-Token: session-secret".to_string(),
            "X-Trace-ID: trace-123".to_string(),
        ],
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{source_address}/source.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(151),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("redirect.bin"),
    )
    .expect("create cross-origin redirect command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("cross-origin redirect download should not hang")
        .expect("cross-origin redirect download should complete");
    assert_eq!(
        std::fs::read(directory.path().join("redirect.bin")).expect("read redirect output"),
        b"ok\n"
    );
    source.await.expect("redirect source fixture should finish");
    target.await.expect("redirect target fixture should finish");
}

#[tokio::test]
async fn conditional_get_304_completes_without_location() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind conditional GET fixture");
    let address = listener.local_addr().expect("read conditional GET address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept conditional GET");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read conditional GET request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /cached.bin HTTP/1.1\r\n"));
        assert!(
            request
                .lines()
                .any(|line| line.to_ascii_lowercase().starts_with("if-modified-since:")),
            "conditional GET must send If-Modified-Since: {request}"
        );
        stream
            .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
            .await
            .expect("write 304 response");
    });

    let directory = tempfile::tempdir().expect("create conditional GET directory");
    let output = directory.path().join("cached.bin");
    std::fs::write(&output, b"cached bytes").expect("create cached output");
    let options = DownloadOptions {
        allow_overwrite: true,
        conditional_get: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/cached.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(13),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("cached.bin"),
    )
    .expect("create conditional GET command");

    tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("conditional GET should not hang")
        .expect("304 should complete the cached download");
    assert_eq!(
        std::fs::read(&output).expect("read cached output"),
        b"cached bytes"
    );

    server.await.expect("conditional GET fixture should finish");
}

#[tokio::test]
async fn unconditional_304_is_rejected_as_http_protocol_error() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unconditional 304 fixture");
    let address = listener
        .local_addr()
        .expect("read unconditional 304 address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept unconditional 304");
        let mut request = vec![0; 4096];
        let bytes = stream
            .read(&mut request)
            .await
            .expect("read unconditional 304 request");
        let request = String::from_utf8_lossy(&request[..bytes]);
        assert!(request.starts_with("GET /cached.bin HTTP/1.1\r\n"));
        assert!(!request.lines().any(|line| {
            let lower = line.to_ascii_lowercase();
            lower.starts_with("if-modified-since:") || lower.starts_with("if-none-match:")
        }));
        stream
            .write_all(b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n")
            .await
            .expect("write unconditional 304 response");
    });

    let directory = tempfile::tempdir().expect("create unconditional 304 directory");
    let output = directory.path().join("cached.bin");
    std::fs::write(&output, b"cached bytes").expect("create cached output");
    let options = DownloadOptions {
        allow_overwrite: true,
        dir: Some(directory.path().to_string_lossy().into_owned()),
        max_retries: 1,
        use_head: false,
        ..DownloadOptions::default()
    };
    let uri = format!("http://{address}/cached.bin");
    let mut command = DownloadCommand::new(
        GroupId::new(14),
        &uri,
        &options,
        Some(directory.path().to_string_lossy().as_ref()),
        Some("cached.bin"),
    )
    .expect("create unconditional 304 command");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("unconditional 304 should not hang")
        .expect_err("unconditional 304 must be rejected");
    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
            if message.contains("304")
    ));
    assert_eq!(
        std::fs::read(&output).expect("read cached output"),
        b"cached bytes"
    );

    server
        .await
        .expect("unconditional 304 fixture should finish");
}
