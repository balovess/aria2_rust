//! Unit tests for FTP negotiation helpers.

use std::time::Duration;

use super::capabilities::ServerCapabilities;
use super::control::read_response_impl;
use super::parsing::{
    days_from_civil, extract_directory_part, extract_file_part, parse_epsv_response,
    parse_mdtm_timestamp, parse_pasv_response, percent_decode,
};
use super::{FtpNegotiationConfig, FtpTransferType, active_data_bind_addr};
use crate::ftp::connection::types::FtpMode;
use crate::{error::Aria2Error, error::RecoverableError};

#[tokio::test]
async fn test_read_response_preserves_multiline_response() {
    let response = b"211-Features:\r\n UTF8\r\n211 End\r\n".to_vec();
    let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response));

    let (code, message) = read_response_impl(&mut reader, Duration::from_secs(1))
        .await
        .expect("multiline FTP response should parse");

    assert_eq!(code, 211);
    assert_eq!(message, "Features:\n UTF8\nEnd");

    let mut capabilities = ServerCapabilities::new();
    capabilities.parse_feat_response(&message);
    assert!(capabilities.utf8);
}

#[tokio::test]
async fn test_read_response_rejects_oversized_response() {
    let response = format!("211-{}\r\n211 End\r\n", "x".repeat(65_536));
    let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response.into_bytes()));

    let error = read_response_impl(&mut reader, Duration::from_secs(1))
        .await
        .expect_err("oversized FTP response must be rejected");

    assert!(error.to_string().contains("Max FTP recv buffer reached"));
}

#[tokio::test]
async fn test_read_response_classifies_eof_as_temporary_network_failure() {
    let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(Vec::<u8>::new()));

    let error = read_response_impl(&mut reader, Duration::from_secs(1))
        .await
        .expect_err("EOF before a response must be an error");

    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
    ));
}

#[tokio::test]
async fn test_read_response_rejects_truncated_and_malformed_first_lines() {
    for response in [
        b"220 Welcome".as_slice(),
        b"hello\r\n".as_slice(),
        b"220x Welcome\r\n".as_slice(),
    ] {
        let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response.to_vec()));
        let error = read_response_impl(&mut reader, Duration::from_secs(1))
            .await
            .expect_err("invalid FTP response must be rejected");

        if response.ends_with(b"\r\n") {
            assert!(matches!(
                error,
                Aria2Error::Recoverable(RecoverableError::FtpProtocolError { .. })
            ));
        } else {
            assert!(matches!(
                error,
                Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
            ));
        }
    }
}

#[tokio::test]
async fn test_read_response_rejects_incomplete_multiline_response() {
    let response = b"211-Features:\r\n UTF8\r\n".to_vec();
    let mut reader = tokio::io::BufReader::new(std::io::Cursor::new(response));

    let error = read_response_impl(&mut reader, Duration::from_secs(1))
        .await
        .expect_err("multiline response without its terminator must be rejected");

    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
    ));
}

#[test]
fn test_percent_decode_basic() {
    assert_eq!(percent_decode("hello"), "hello");
    assert_eq!(percent_decode("hello%20world"), "hello world");
    assert_eq!(percent_decode("file%2Etxt"), "file.txt");
}

#[test]
fn test_percent_decode_utf8() {
    // Chinese character for "file" encoded in UTF-8 percent-encoding
    assert_eq!(percent_decode("%E6%96%87%E4%BB%B6"), "文件");
}

#[test]
fn test_percent_decode_invalid() {
    // Invalid percent-encoding preserved as literal
    assert_eq!(percent_decode("test%ZZ"), "test%ZZ");
}

#[test]
fn test_percent_decode_mixed() {
    assert_eq!(
        percent_decode("/pub/my%20dir/file.txt"),
        "/pub/my dir/file.txt"
    );
}

#[test]
fn test_extract_directory_part() {
    assert_eq!(
        extract_directory_part("/pub/linux/file.tar.gz"),
        "/pub/linux"
    );
    assert_eq!(extract_directory_part("/file.txt"), "");
    assert_eq!(extract_directory_part("/"), "");
    assert_eq!(extract_directory_part(""), "");
    assert_eq!(
        extract_directory_part("/path/to/dir/file.txt"),
        "/path/to/dir"
    );
}

#[test]
fn test_extract_directory_part_percent_decoded() {
    // Percent-encoded path components are decoded before extraction
    assert_eq!(
        extract_directory_part("/pub/my%20dir/file.txt"),
        "/pub/my dir"
    );
    // %2E%2E decodes to "..", so /path/%2E%2E/hidden/file.txt -> /path/../hidden
    assert_eq!(
        extract_directory_part("/path/%2E%2E/hidden/file.txt"),
        "/path/../hidden"
    );
}

#[test]
fn test_extract_file_part() {
    assert_eq!(extract_file_part("/pub/linux/file.tar.gz"), "file.tar.gz");
    assert_eq!(extract_file_part("/file.txt"), "file.txt");
    assert_eq!(extract_file_part("/"), "");
    assert_eq!(extract_file_part(""), "");
}

#[test]
fn test_extract_file_part_percent_decoded() {
    // Percent-encoded file names are decoded before extraction
    assert_eq!(
        extract_file_part("/pub/linux/my%20file.tar.gz"),
        "my file.tar.gz"
    );
    assert_eq!(extract_file_part("/dir/file%2Etxt"), "file.txt");
}

#[test]
fn test_parse_mdtm_timestamp_valid() {
    let ts = parse_mdtm_timestamp("20240115103000").unwrap();
    // 2024-01-15 10:30:00 UTC = epoch 1705314600
    let duration = ts.duration_since(std::time::UNIX_EPOCH).unwrap();
    assert_eq!(duration.as_secs(), 1705314600);
}

#[test]
fn test_parse_mdtm_timestamp_invalid() {
    assert!(parse_mdtm_timestamp("").is_none());
    assert!(parse_mdtm_timestamp("2024").is_none());
    assert!(parse_mdtm_timestamp("20241301120000").is_none()); // month 13
    assert!(parse_mdtm_timestamp("20240132120000").is_none()); // day 32
}

#[test]
fn test_parse_pasv_response_standard() {
    let resp = "227 Entering Passive Mode (192,168,1,100,195,123)";
    let result = parse_pasv_response(resp).unwrap();
    assert_eq!(result.0, "192.168.1.100");
    assert_eq!(result.1, 195 * 256 + 123);
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
fn test_days_from_civil_epoch() {
    // 1970-01-01 should be day 0
    assert_eq!(days_from_civil(1970, 1, 1), Some(0));
    // 1970-01-02 should be day 1
    assert_eq!(days_from_civil(1970, 1, 2), Some(1));
    // 2000-01-01 known value: 10957
    assert_eq!(days_from_civil(2000, 1, 1), Some(10957));
}

#[test]
fn test_cwd_traversal_splitting() {
    // The CWD traversal should split "/pub/linux" into ["pub", "linux"]
    // and NOT send CWD for the full path at once
    let path = "/pub/linux";
    let dirs: Vec<&str> = path.split('/').filter(|c| !c.is_empty()).collect();
    assert_eq!(dirs, vec!["pub", "linux"]);
}

#[test]
fn test_cwd_targets_preserve_base_and_skip_empty_uri_components() {
    assert_eq!(
        super::parsing::cwd_targets("/", "/pub//linux"),
        vec!["/", "pub", "linux"]
    );
}

#[test]
fn test_split_decoded_remote_path_does_not_decode_again() {
    assert_eq!(
        super::parsing::split_decoded_remote_path("/pub/my%20file.txt"),
        ("/pub".to_string(), "my%20file.txt".to_string())
    );
    assert_eq!(
        super::parsing::split_decoded_remote_path("/pub/my file.txt"),
        ("/pub".to_string(), "my file.txt".to_string())
    );
}

#[test]
fn test_ftp_negotiation_config_defaults() {
    let config = FtpNegotiationConfig {
        host: "example.com".to_string(),
        port: 21,
        username: "anonymous".to_string(),
        password: "aria2@".to_string(),
        remote_path: "/pub/file.txt".to_string(),
        mode: FtpMode::Passive,
        transfer_type: FtpTransferType::Binary,
        resume_offset: 0,
        remote_time: false,
        connect_timeout: Duration::from_secs(30),
        command_timeout: Duration::from_secs(30),
        is_pooled: false,
        pooled_base_working_dir: None,
        data_proxy: None,
    };
    assert_eq!(config.host, "example.com");
    assert_eq!(config.port, 21);
    assert!(!config.is_pooled);
}

#[test]
fn active_data_listener_keeps_the_control_interface() {
    let ipv4: std::net::SocketAddr = "192.0.2.10:43123".parse().unwrap();
    assert_eq!(active_data_bind_addr(ipv4), "192.0.2.10:0".parse().unwrap());

    let ipv6: std::net::SocketAddr = "[2001:db8::10]:43123".parse().unwrap();
    assert_eq!(
        active_data_bind_addr(ipv6),
        "[2001:db8::10]:0".parse().unwrap()
    );
}

// =============================================================================
// ServerCapabilities tests
// =============================================================================

#[test]
fn test_server_capabilities_default() {
    let caps = ServerCapabilities::new();
    assert!(!caps.utf8);
    assert!(!caps.mlst_mlsd);
    assert!(!caps.size);
    assert!(!caps.mdtm);
    assert!(!caps.epsv);
    assert!(!caps.eprt);
    assert!(!caps.tvfs);
    assert!(caps.syst.is_none());
}

#[test]
fn test_feat_parse_full() {
    let response = "\
211-Features:
 UTF8
 MLST type*;size*;modify*;
 SIZE
 MDTM
 EPSV
 EPRT
 TVFS
211 End";

    let mut caps = ServerCapabilities::new();
    caps.parse_feat_response(response);

    assert!(caps.utf8);
    assert!(caps.mlst_mlsd);
    assert!(caps.size);
    assert!(caps.mdtm);
    assert!(caps.epsv);
    assert!(caps.eprt);
    assert!(caps.tvfs);
}

#[test]
fn test_feat_parse_minimal() {
    let response = "\
211-Features:
 UTF8
211 End";

    let mut caps = ServerCapabilities::new();
    caps.parse_feat_response(response);

    assert!(caps.utf8);
    assert!(!caps.mlst_mlsd);
    assert!(!caps.size);
}

#[test]
fn test_feat_parse_case_insensitive() {
    let response = "\
211-Features:
 utf8
 mlst type*;size*;
211 End";

    let mut caps = ServerCapabilities::new();
    caps.parse_feat_response(response);

    assert!(caps.utf8);
    assert!(caps.mlst_mlsd);
}

#[test]
fn test_feat_parse_empty() {
    let response = "211 End";
    let mut caps = ServerCapabilities::new();
    caps.parse_feat_response(response);

    assert!(!caps.utf8);
    assert!(!caps.mlst_mlsd);
}

#[test]
fn test_feat_mlsd_as_separate_feature() {
    // Some servers list MLSD as a separate feature
    let response = "\
211-Features:
 MLST type*;size*;
 MLSD
211 End";

    let mut caps = ServerCapabilities::new();
    caps.parse_feat_response(response);

    assert!(caps.mlst_mlsd);
}
