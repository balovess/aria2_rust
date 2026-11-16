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
