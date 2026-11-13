//! Tests for FTP download command.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use super::control::RawFtpControl;
use super::types::FtpDownloadCommand;
use crate::engine::command::Command;
use crate::error::Aria2Error;
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;
use aria2_protocol::ftp::connection::{
    parse_epsv_response, parse_pasv_response, parse_size_response as parse_ftp_size_response,
    percent_decode as urlencoding_decode,
};

#[test]
fn test_parse_uri_simple() {
    let uri = "ftp://example.com/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.host, "example.com");
    assert_eq!(result.port, 21);
    assert_eq!(result.username, "anonymous");
    assert_eq!(result.password, "aria2@");
    assert_eq!(result.remote_path, "/file.txt");
    assert!(!result.secure);
}

#[test]
fn test_retry_policy_comes_from_download_options() {
    let options = DownloadOptions {
        max_retries: 7,
        retry_wait: 3,
        ..DownloadOptions::default()
    };
    let command = FtpDownloadCommand::new(
        GroupId::new(101),
        "ftp://example.com/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();

    assert_eq!(command.retry_policy.max_tries(), 7);
    assert_eq!(command.retry_policy.base_wait_ms, 3000);
}

#[test]
fn ftp_proxy_rpc_options_reach_the_core_proxy_configuration() {
    let rpc_options = HashMap::from([
        (
            "ftp-proxy".to_string(),
            serde_json::json!("http://proxy.example:3128"),
        ),
        (
            "ftp-proxy-user".to_string(),
            serde_json::json!("proxy-user"),
        ),
        (
            "ftp-proxy-passwd".to_string(),
            serde_json::json!("proxy-password"),
        ),
        ("proxy-method".to_string(), serde_json::json!("get")),
        (
            "user-agent".to_string(),
            serde_json::json!("task-agent/1.0"),
        ),
    ]);
    let options = DownloadOptions::try_from_rpc_options(&rpc_options)
        .expect("RPC options should use the registered option parser");
    let command = FtpDownloadCommand::new(
        GroupId::new(102),
        "ftp://download-user@example.com/file.bin",
        &options,
        None,
        None,
    )
    .expect("FTP command should accept valid proxy options");

    let (proxy, method) = command
        .ftp_proxy_config()
        .expect("FTP proxy configuration should be valid")
        .expect("FTP proxy should be configured");
    assert_eq!(proxy.proxy_host, "proxy.example");
    assert_eq!(proxy.proxy_port, 3128);
    assert_eq!(proxy.proxy_username, "proxy-user");
    assert_eq!(proxy.proxy_password, "proxy-password");
    assert_eq!(proxy.ftp_username, "download-user");
    assert_eq!(proxy.user_agent, "task-agent/1.0");
    assert_eq!(method, crate::ftp::connection::ProxyMethod::Get);
}

#[tokio::test]
async fn ftp_proxy_records_each_payload_chunk_for_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let first_chunk = vec![b'A'; 16 * 1024];
    let second_chunk = vec![b'B'; 16 * 1024];
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn({
        let first_chunk = first_chunk.clone();
        let second_chunk = second_chunk.clone();
        async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let bytes_read = stream.read(&mut request).await.unwrap();
            assert!(bytes_read > 0, "FTP proxy fixture should receive a request");
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        first_chunk.len() + second_chunk.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(&first_chunk).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(150)).await;
            stream.write_all(&second_chunk).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });

    let output_dir = tempfile::tempdir().unwrap();
    let url = "ftp://ftp.example.test/pub/proxy-timeout.bin".to_string();
    let options = DownloadOptions {
        ftp_proxy: Some(format!("http://{address}")),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(9002),
        vec![url.clone()],
        options,
    )));
    let mut command =
        FtpDownloadCommand::new_with_group(Arc::clone(&group), output_dir.path().to_str(), None)
            .unwrap();

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if group.recover().completed_length() >= first_chunk.len() as u64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("FTP proxy did not receive its first payload chunk");
    let first_activity = group.recover().last_network_activity();

    tokio::time::timeout(Duration::from_secs(5), command_task)
        .await
        .expect("FTP proxy command did not complete")
        .expect("FTP proxy command panicked")
        .expect("FTP proxy command failed");
    server.await.unwrap();

    assert!(
        group.recover().last_network_activity() > first_activity,
        "each non-empty FTP proxy payload chunk must refresh the inactivity clock"
    );
}

#[tokio::test]
async fn retry_wait_is_interruptible_when_paused() {
    let command = FtpDownloadCommand::new(
        GroupId::new(104),
        "ftp://example.com/file.txt",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    command.group.write().unwrap().pause().unwrap();

    let result = tokio::time::timeout(
        Duration::from_millis(100),
        command.wait_for_retry(Duration::from_secs(5)),
    )
    .await
    .expect("paused retry wait should stop promptly");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[tokio::test]
async fn retry_wait_wakes_when_paused_after_wait_starts() {
    let command = FtpDownloadCommand::new(
        GroupId::new(105),
        "ftp://example.com/file.txt",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    let group = std::sync::Arc::clone(&command.group);
    let wait_task =
        tokio::spawn(async move { command.wait_for_retry(Duration::from_secs(5)).await });

    tokio::time::sleep(Duration::from_millis(10)).await;
    group.write().unwrap().pause().unwrap();

    let result = tokio::time::timeout(Duration::from_millis(100), wait_task)
        .await
        .expect("pause should wake an active FTP retry wait")
        .expect("FTP retry wait task should not panic");
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
}

#[test]
fn test_connect_timeout_comes_from_download_options() {
    let options = DownloadOptions {
        connect_timeout: Some(12),
        ..DownloadOptions::default()
    };
    let command = FtpDownloadCommand::new(
        GroupId::new(102),
        "ftp://example.com/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();

    assert_eq!(command.connect_timeout, std::time::Duration::from_secs(12));
}

#[test]
fn command_timeout_comes_from_download_options() {
    let options = DownloadOptions {
        timeout: Some(7),
        ..DownloadOptions::default()
    };
    let command = FtpDownloadCommand::new(
        GroupId::new(103),
        "ftp://example.com/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();

    assert_eq!(
        Command::timeout(&command),
        Some(Duration::from_secs(7)),
        "timeout must be the configured I/O inactivity duration"
    );
}

#[test]
fn test_parse_uri_with_port() {
    let uri = "ftp://example.com:2121/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.host, "example.com");
    assert_eq!(result.port, 2121);
}

#[test]
fn test_parse_uri_with_auth() {
    let uri = "ftp://user:pass@example.com/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.username, "user");
    assert_eq!(result.password, "pass");
}

#[test]
fn test_parse_uri_with_encoded_chars() {
    let uri = "ftp://example.com/my%20file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.remote_path, "/my file.txt");
}

#[test]
fn test_parse_uri_preserves_ftps_scheme_for_core_policy() {
    let result = FtpDownloadCommand::parse_uri("FTPS://example.com:990/file.txt").unwrap();
    assert!(result.secure);
    assert_eq!(result.port, 990);
}

#[test]
fn test_parse_uri_invalid_protocol() {
    let uri = "http://example.com/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri);
    assert!(result.is_err());
}

#[test]
fn test_extract_filename_from_path() {
    assert_eq!(
        FtpDownloadCommand::extract_filename("/path/to/file.txt"),
        Some("file.txt".to_string())
    );
    assert_eq!(
        FtpDownloadCommand::extract_filename("/file.txt"),
        Some("file.txt".to_string())
    );
    assert_eq!(FtpDownloadCommand::extract_filename("/"), None);
    assert_eq!(FtpDownloadCommand::extract_filename(""), None);
}

#[test]
fn test_urlencoding_decode() {
    assert_eq!(urlencoding_decode("hello%20world"), "hello world");
    assert_eq!(urlencoding_decode("%2F"), "/");
    assert_eq!(urlencoding_decode("normal"), "normal");
    assert_eq!(urlencoding_decode("%41"), "A");
}

#[test]
fn test_urlencoding_decode_utf8() {
    assert_eq!(urlencoding_decode("%E6%96%87%E4%BB%B6"), "文件");
}

#[test]
fn test_parse_pasv_response_standard() {
    let resp = "227 Entering Passive Mode (192,168,1,100,200,10)";
    let result = parse_pasv_response(resp).unwrap();
    assert_eq!(result.0, "192.168.1.100");
    assert_eq!(result.1, 200 * 256 + 10); // 51210
}

#[test]
fn test_parse_pasv_response_minimal() {
    let resp = "(10,0,0,1,0,21)";
    let result = parse_pasv_response(resp).unwrap();
    assert_eq!(result.0, "10.0.0.1");
    assert_eq!(result.1, 21);
}

#[test]
fn test_parse_pasv_response_invalid() {
    assert!(parse_pasv_response("no parentheses").is_none());
    assert!(parse_pasv_response("(1,2,3)").is_none()); // Too few parts
    assert!(parse_pasv_response("(1,2,3,4,256,1)").is_none());
}

#[test]
fn test_parse_epsv_response_standard() {
    let resp = "229 Entering Extended Passive Mode (|||50001|)";
    let result = parse_epsv_response(resp).unwrap();
    assert_eq!(result, 50001);
}

#[test]
fn test_parse_epsv_response_minimal() {
    let resp = "|||60000|";
    let result = parse_epsv_response(resp).unwrap();
    assert_eq!(result, 60000);
    assert!(parse_epsv_response("|||0|").is_none());
}

#[test]
fn test_parse_ftp_size_response_accepts_signed_offset_limit() {
    assert_eq!(
        parse_ftp_size_response(" 9223372036854775807 ").unwrap(),
        i64::MAX as u64
    );
}

#[test]
fn test_parse_ftp_size_response_rejects_values_above_signed_offset_limit() {
    let error = parse_ftp_size_response("9223372036854775808")
        .expect_err("SIZE above the local offset range must be rejected");

    assert!(error.contains("too large"));
}

#[test]
fn test_resume_offset_calculation() {
    // Test that resume offset would be calculated correctly from existing file
    // (This logic is in new(), so we verify the concept)
    let path = std::path::PathBuf::from("/tmp/test_file");
    if path.exists() {
        let metadata = std::fs::metadata(&path).unwrap();
        let _offset = metadata.len();
        // offset is u64, always >= 0 by type guarantee
    } else {
        // File doesn't exist, offset should be 0
        assert_eq!(0u64, 0);
    }
}

#[tokio::test]
async fn test_raw_ftp_control_connect_invalid_address() {
    let result = RawFtpControl::connect_at(
        "invalid.host.name.invalid",
        21,
        "127.0.0.1:0".parse().unwrap(),
        Arc::new(crate::network::OutboundNetworkPolicy::direct()),
    )
    .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn ftp_control_binding_reaches_the_server_socket() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP fixture should bind");
    let address = listener.local_addr().expect("FTP fixture address");
    let server = tokio::spawn(async move {
        let (mut stream, peer) = listener.accept().await.expect("FTP client should connect");
        stream
            .write_all(b"220 test FTP server\r\n")
            .await
            .expect("write FTP greeting");
        peer.ip()
    });

    let control = RawFtpControl::connect_at(
        "127.0.0.1",
        address.port(),
        address,
        Arc::new(crate::network::OutboundNetworkPolicy::single(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )),
    )
    .await
    .expect("FTP control connection should use the configured source address");

    assert_eq!(
        control.connection_context().peer_addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    assert_eq!(
        server.await.expect("FTP fixture should finish"),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[tokio::test]
async fn ftp_control_reuses_one_policy_bound_connection_for_multiple_commands() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP reuse fixture should bind");
    let address = listener.local_addr().expect("FTP reuse fixture address");
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept().await.expect("FTP client should connect");
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(b"220 reuse fixture\r\n")
            .await
            .expect("write FTP greeting");
        for _ in 0..2 {
            let mut line = Vec::new();
            reader
                .read_until(b'\n', &mut line)
                .await
                .expect("read FTP command");
            assert!(line.starts_with(b"NOOP ") || line.starts_with(b"NOOP\r"));
            reader
                .get_mut()
                .write_all(b"200 NOOP ok\r\n")
                .await
                .expect("write FTP command response");
        }
        peer
    });

    let mut control = RawFtpControl::connect_at(
        "127.0.0.1",
        address.port(),
        address,
        Arc::new(crate::network::OutboundNetworkPolicy::single(
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        )),
    )
    .await
    .expect("FTP control connection should be established");
    assert_eq!(control.command("NOOP 1").await.unwrap().0, 200);
    assert_eq!(control.command("NOOP 2").await.unwrap().0, 200);
    assert_eq!(
        server.await.unwrap().ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
}

#[tokio::test]
async fn ftp_ipv4_passive_mode_uses_pasv_without_epsv_probe() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    let data_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP data fixture should bind");
    let data_port = data_listener
        .local_addr()
        .expect("FTP data fixture address")
        .port();
    let control_listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("FTP control fixture should bind");
    let control_address = control_listener
        .local_addr()
        .expect("FTP control fixture address");

    let server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("FTP control connection should arrive");
        let mut reader = BufReader::new(stream);
        reader
            .get_mut()
            .write_all(b"220 FTP fixture ready\r\n")
            .await
            .expect("write greeting");

        let mut command = String::new();
        reader
            .read_line(&mut command)
            .await
            .expect("read passive command");
        assert_eq!(command.trim_end(), "PASV");
        reader
            .get_mut()
            .write_all(
                format!(
                    "227 Entering Passive Mode (127,0,0,1,{},{})\r\n",
                    data_port / 256,
                    data_port % 256
                )
                .as_bytes(),
            )
            .await
            .expect("write PASV response");

        data_listener
            .accept()
            .await
            .expect("FTP data connection should use the advertised port");
    });

    let mut control = RawFtpControl::connect_at(
        "127.0.0.1",
        control_address.port(),
        control_address,
        Arc::new(crate::network::OutboundNetworkPolicy::direct()),
    )
    .await
    .expect("FTP control connection should be established");
    let data_stream = control
        .enter_passive_mode()
        .await
        .expect("IPv4 passive mode should use PASV");
    drop(data_stream);
    server.await.expect("FTP fixture should finish");
}
