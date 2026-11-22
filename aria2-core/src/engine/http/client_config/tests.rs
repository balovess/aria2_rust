use super::*;

#[test]
fn proxy_log_target_omits_credentials() {
    assert_eq!(
        proxy_log_target("https://user:secret@[2001:db8::1]:8443"),
        "https://[2001:db8::1]:8443"
    );
    assert_eq!(
        proxy_log_target("unknown://user:secret@proxy.example:3128"),
        "<invalid proxy URL>"
    );
}

#[tokio::test]
async fn proxy_request_routing_uses_exact_no_proxy_hostname_matching() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    crate::http::client_pool::ensure_rustls_provider();

    let target_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target_listener.local_addr().unwrap();
    let target_task = tokio::spawn(async move {
        let (mut stream, _) = target_listener.accept().await.unwrap();
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\ndirect")
            .await
            .unwrap();
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = proxy_listener.local_addr().unwrap();
    let proxy_task = tokio::spawn(async move {
        let (mut stream, _) = proxy_listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let read = stream.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "proxy request ended before its headers");
            request.extend_from_slice(&chunk[..read]);
            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nproxy!")
            .await
            .unwrap();
    });

    let no_proxy = "example.test";
    let proxy_url = format!("http://{proxy_address}");
    let options = DownloadOptions {
        all_proxy: Some(proxy_url.clone()),
        no_proxy: Some(no_proxy.to_string()),
        ..DownloadOptions::default()
    };
    let uri = format!(
        "http://download.example.test:{}/file",
        target_address.port()
    );
    assert_eq!(
        http_proxy_origin(&uri, &options),
        Some(("127.0.0.1".to_string(), proxy_address.port()))
    );

    let proxy =
        build_reqwest_proxy(ProxyTarget::All, &proxy_url, None, None, Some(no_proxy)).unwrap();
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .resolve("download.example.test", target_address)
        .build()
        .unwrap();
    let response = client.get(uri).send().await.unwrap();
    assert_eq!(response.text().await.unwrap(), "proxy!");

    proxy_task.await.unwrap();
    target_task.abort();
    let _ = target_task.await;
}

#[test]
fn download_connect_timeout_uses_the_request_option() {
    let options = DownloadOptions {
        connect_timeout: Some(47),
        ..DownloadOptions::default()
    };
    assert_eq!(http_connect_timeout(&options), Duration::from_secs(47));
}

#[test]
fn download_connect_timeout_has_a_fallback_for_direct_core_callers() {
    let options = DownloadOptions::default();
    assert_eq!(
        http_connect_timeout(&options),
        Duration::from_secs(constants::HTTP_DEFAULT_CONNECT_TIMEOUT_SECS)
    );
}

#[test]
fn multi_ip_client_count_is_opt_in_and_respects_request_concurrency() {
    let mut options = DownloadOptions {
        split: Some(8),
        max_connection_per_server: Some(4),
        http_version: crate::http::HttpVersion::Http11,
        use_multiple_ips: false,
        ..DownloadOptions::default()
    };
    assert_eq!(multi_ip_client_count(&options, 8), 0);

    options.use_multiple_ips = true;
    assert_eq!(multi_ip_client_count(&options, 8), 4);
    assert_eq!(multi_ip_client_count(&options, 2), 2);
    assert_eq!(multi_ip_client_count(&options, 1), 0);

    options.http_version = crate::http::HttpVersion::Http2;
    assert_eq!(
        multi_ip_client_count(&options, 8),
        constants::DEFAULT_HTTP2_SESSIONS_PER_SERVER
    );
}

#[test]
fn rotated_addresses_prefers_each_client_address_and_retains_fallback_order() {
    let addresses: Vec<SocketAddr> = ["127.0.0.1:443", "127.0.0.2:443", "127.0.0.3:443"]
        .into_iter()
        .map(|address| address.parse().unwrap())
        .collect();

    assert_eq!(rotated_addresses(&addresses, 0), addresses);
    assert_eq!(
        rotated_addresses(&addresses, 1),
        vec![addresses[1], addresses[2], addresses[0]]
    );
    assert_eq!(rotated_addresses(&addresses, 3), addresses);
    assert!(rotated_addresses(&[], 1).is_empty());
}

#[test]
fn mixed_address_family_uses_preferred_family_when_it_has_enough_addresses() {
    let options = DownloadOptions {
        split: Some(8),
        max_connection_per_server: Some(8),
        use_multiple_ips: true,
        ..DownloadOptions::default()
    };
    let addresses: Vec<SocketAddr> = ["192.0.2.1:443", "192.0.2.2:443", "[2001:db8::1]:443"]
        .into_iter()
        .map(|address| address.parse().unwrap())
        .collect();

    let selected = select_multi_ip_addresses(
        &options,
        &addresses,
        &crate::network::OutboundNetworkPolicy::direct(),
    );
    assert_eq!(selected, vec![addresses[0], addresses[1]]);
}

#[test]
fn mixed_address_family_fills_from_other_family_when_preferred_has_one_address() {
    let options = DownloadOptions {
        split: Some(8),
        max_connection_per_server: Some(8),
        use_multiple_ips: true,
        ..DownloadOptions::default()
    };
    let addresses: Vec<SocketAddr> = ["192.0.2.1:443", "[2001:db8::1]:443", "[2001:db8::2]:443"]
        .into_iter()
        .map(|address| address.parse().unwrap())
        .collect();

    let selected = select_multi_ip_addresses(
        &options,
        &addresses,
        &crate::network::OutboundNetworkPolicy::direct(),
    );
    assert_eq!(selected, addresses);
}
