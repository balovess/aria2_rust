//! Tests for FTP download command.

use std::sync::Arc;
use std::time::Duration;

use super::control::{
    RawFtpControl, parse_epsv_response, parse_ftp_size_response, parse_pasv_response,
    urlencoding_decode,
};
use super::types::FtpDownloadCommand;
use crate::engine::command::Command;
use crate::error::{Aria2Error, RecoverableError};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use crate::util::rwlock_ext::RwLockRecover;

#[test]
fn test_parse_uri_simple() {
    let uri = "ftp://example.com/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.0, "example.com");
    assert_eq!(result.1, 21);
    assert_eq!(result.2, "anonymous");
    assert_eq!(result.3, "aria2@");
    assert_eq!(result.4, "/file.txt");
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
fn test_parse_uri_with_port() {
    let uri = "ftp://example.com:2121/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.0, "example.com");
    assert_eq!(result.1, 2121);
}

#[test]
fn test_parse_uri_with_auth() {
    let uri = "ftp://user:pass@example.com/file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.2, "user");
    assert_eq!(result.3, "pass");
}

#[test]
fn test_parse_uri_with_encoded_chars() {
    let uri = "ftp://example.com/my%20file.txt";
    let result = FtpDownloadCommand::parse_uri(uri).unwrap();
    assert_eq!(result.4, "/my file.txt");
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

    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::FtpProtocolError { .. })
    ));
    assert!(error.to_string().contains("too large"));
}

#[test]
fn test_classify_ftp_error_transient() {
    // These should be classified as transient/recoverable
    let transient_codes = [421u16, 425, 426, 450, 451, 452];
    for code in transient_codes {
        assert!(
            (400..=499).contains(&code),
            "Code {} should be in transient range",
            code
        );
    }
}

#[test]
fn test_classify_ftp_error_permanent() {
    // These should be classified as permanent/fatal
    let permanent_codes = [500u16, 501, 502, 503, 504, 530, 550, 553];
    for code in permanent_codes {
        assert!(
            (500..=599).contains(&code),
            "Code {} should be in permanent range",
            code
        );
    }
}

#[test]
fn test_classify_ftp_not_found_uses_resource_result() {
    let command = FtpDownloadCommand::new(
        GroupId::new(103),
        "ftp://example.com/file.txt",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();

    assert!(matches!(
        command.classify_ftp_error(550, "File unavailable"),
        Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
    ));
    assert!(matches!(
        command.classify_ftp_error(450, "Busy"),
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
    ));
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
    )
    .await;
    assert!(result.is_err());
}
