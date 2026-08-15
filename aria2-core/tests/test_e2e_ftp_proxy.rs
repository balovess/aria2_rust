use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aria2_core::engine::command::Command;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};

mod fixtures;
use fixtures::mock_ftp_server::MockFtpServer;

const PROXY_BODY: &[u8] = b"proxy-owned FTP payload\n";

async fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "proxy client closed before request headers");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(
            bytes.len() < 16 * 1024,
            "proxy request headers are too large"
        );
    }
    String::from_utf8(bytes).unwrap()
}

async fn start_get_proxy(
    expected_authorization: String,
) -> (
    std::net::SocketAddr,
    Arc<Mutex<String>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let request_capture = Arc::new(Mutex::new(String::new()));
    let capture = Arc::clone(&request_capture);
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        *capture.lock().unwrap() = request.clone();
        assert!(request.starts_with("GET ftp://"));
        assert!(request.contains("Proxy-Authorization: "));
        assert!(request.contains(&format!("Proxy-Authorization: {}", expected_authorization)));
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            PROXY_BODY.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.write_all(PROXY_BODY).await.unwrap();
        stream.shutdown().await.unwrap();
    });
    (address, request_capture, task)
}

async fn start_tunnel_proxy() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let request = read_request(&mut client).await;
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_else(|| panic!("invalid CONNECT request: {request}"));
        let mut upstream = TcpStream::connect(target).await.unwrap();
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        copy_bidirectional(&mut client, &mut upstream)
            .await
            .unwrap();
    });
    (address, task)
}

fn options(proxy_url: String) -> DownloadOptions {
    DownloadOptions {
        ftp_proxy: Some(proxy_url),
        ftp_proxy_user: Some("proxy-user".to_string()),
        ftp_proxy_passwd: Some("proxy-pass".to_string()),
        ..DownloadOptions::default()
    }
}

#[tokio::test]
async fn ftp_proxy_get_uses_absolute_ftp_uri_and_streams_response() {
    use base64::Engine;

    let expected_authorization = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(b"proxy-user:proxy-pass")
    );
    let (proxy_addr, request_capture, proxy_task) = start_get_proxy(expected_authorization).await;
    let output_dir = tempfile::tempdir().unwrap();
    let url = "ftp://ftp.example.test/pub/proxy.bin";
    let proxy_url = format!("http://{}", proxy_addr);
    let mut command = FtpDownloadCommand::new(
        GroupId::new(700),
        url,
        &options(proxy_url),
        output_dir.path().to_str(),
        None,
    )
    .unwrap();

    command.execute().await.unwrap();
    proxy_task.await.unwrap();

    assert_eq!(
        std::fs::read(output_dir.path().join("proxy.bin")).unwrap(),
        PROXY_BODY
    );
    assert!(request_capture.lock().unwrap().contains("GET ftp://"));
    assert!(
        request_capture
            .lock()
            .unwrap()
            .contains("ftp.example.test/pub/proxy.bin HTTP/1.1")
    );
}

#[tokio::test]
async fn ftp_proxy_tunnel_preserves_ftp_control_download() {
    let ftp_server = MockFtpServer::start().await;
    let (proxy_addr, proxy_task) = start_tunnel_proxy().await;
    let output_dir = tempfile::tempdir().unwrap();
    let url = format!(
        "ftp://127.0.0.1:{}/files/small.bin",
        ftp_server.addr().port()
    );
    let mut download_options = options(format!("http://{}", proxy_addr));
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(701),
        vec![url],
        {
            download_options.ftp_proxy_user = None;
            download_options.ftp_proxy_passwd = None;
            download_options
        },
    )));
    group.write().unwrap().set_option_snapshot(HashMap::from([(
        "proxy-method".to_string(),
        serde_json::Value::String("tunnel".to_string()),
    )]));
    let mut command =
        FtpDownloadCommand::new_with_group(group, output_dir.path().to_str(), None).unwrap();

    command.execute().await.unwrap();
    proxy_task.await.unwrap();

    assert_eq!(
        std::fs::read(output_dir.path().join("small.bin")).unwrap(),
        [0xDE, 0xAD, 0xBE, 0xEF]
    );
}
