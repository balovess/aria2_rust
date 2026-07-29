//! Tests for SFTP download command.

use std::sync::Arc;
use std::time::Duration;

use aria2_protocol::sftp::connection::SshError;
use aria2_protocol::sftp::file_ops::FileOpError;

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

use super::types::SftpDownloadCommand;
use super::uri::sftp_path_decode;

#[test]
fn test_sftp_path_decoding() {
    assert_eq!(sftp_path_decode("/normal/path"), "/normal/path");
    assert_eq!(
        sftp_path_decode("/path%20with%20spaces"),
        "/path with spaces"
    );
    // UTF-8 encoded Chinese path: "%E6%96%87%E4%BB%B6" decodes to Chinese characters for "file"
    assert_eq!(sftp_path_decode("/%E6%96%87%E4%BB%B6"), "/\u{6587}\u{4EF6}");
    assert_eq!(sftp_path_decode("%2Froot%2Ftest"), "/root/test");
}

#[test]
fn test_build_ssh_options_with_password() {
    let cmd = create_test_cmd();
    let opts = cmd.build_ssh_options();
    assert_eq!(opts.host, "example.com");
    assert_eq!(opts.port, 2222);
    assert_eq!(opts.username, "testuser");
    assert_eq!(opts.password.as_deref(), Some("secretpass"));
    assert_eq!(
        opts.connect_timeout,
        Duration::from_secs(constants::SFTP_CONNECT_TIMEOUT_SECS)
    );
}

#[test]
fn test_build_ssh_options_without_password() {
    let cmd = SftpDownloadCommand {
        group: Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(99),
            vec!["sftp://user@host/file".to_string()],
            DownloadOptions::default(),
        ))),
        output_path: std::path::PathBuf::from("/tmp/out"),
        started: false,
        completed_bytes: 0,
        host: "host".to_string(),
        port: 22,
        username: "user".to_string(),
        password: None,
        remote_path: "/file".to_string(),
    };
    let opts = cmd.build_ssh_options();
    assert!(opts.password.is_none());
}

#[test]
fn test_map_ssh_auth_error_to_fatal() {
    let err = SshError::AuthFailed {
        method: "password".into(),
        message: "bad pass".into(),
    };
    let mapped = SftpDownloadCommand::map_ssh_error(&err, "h", 22, "/f");
    assert!(matches!(
        mapped,
        Aria2Error::Fatal(FatalError::PermissionDenied { .. })
    ));
}

#[test]
fn test_map_ssh_connect_timeout_to_recoverable() {
    let err = SshError::ConnectTimeout {
        host: "h".into(),
        port: 22,
        timeout_secs: 15,
    };
    let mapped = SftpDownloadCommand::map_ssh_error(&err, "h", 22, "/f");
    assert!(matches!(
        mapped,
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
    ));
}

#[test]
fn test_map_file_not_found_to_fatal() {
    let err = FileOpError::NotFound {
        path: "/missing".to_string(),
    };
    let mapped = SftpDownloadCommand::map_file_op_error(&err, "host", "/missing");
    assert!(matches!(
        mapped,
        Aria2Error::Fatal(FatalError::FileNotFound { .. })
    ));
}

#[test]
fn test_map_permission_denied_to_fatal() {
    let err = FileOpError::PermissionDenied {
        path: "/secret".to_string(),
    };
    let mapped = SftpDownloadCommand::map_file_op_error(&err, "host", "/secret");
    assert!(matches!(
        mapped,
        Aria2Error::Fatal(FatalError::PermissionDenied { .. })
    ));
}

#[test]
fn test_map_network_error_to_recoverable() {
    let err = FileOpError::Network {
        operation: "READ".into(),
        message: "Connection reset".into(),
    };
    let mapped = SftpDownloadCommand::map_file_op_error(&err, "host", "/f");
    assert!(matches!(
        mapped,
        Aria2Error::Recoverable(RecoverableError::TemporaryNetworkFailure { .. })
    ));
}

#[test]
fn test_constants() {
    assert_eq!(constants::SFTP_CONNECT_TIMEOUT_SECS, 15);
    assert_eq!(constants::SFTP_READ_TIMEOUT_SECS, 30);
    assert_eq!(constants::SFTP_COMMAND_TIMEOUT_SECS, 300);
    assert_eq!(constants::SFTP_DISK_WRITE_CHUNK_SIZE, 65536); // 64KB
    assert_eq!(constants::SFTP_SPEED_UPDATE_INTERVAL_MS, 500);
}

/// Helper to create a test command instance
fn create_test_cmd() -> SftpDownloadCommand {
    SftpDownloadCommand {
        group: Arc::new(std::sync::RwLock::new(RequestGroup::new(
            GroupId::new(1),
            vec!["sftp://testuser:secretpass@example.com:2222/path/to/file.zip".to_string()],
            DownloadOptions::default(),
        ))),
        output_path: std::path::PathBuf::from("/tmp/download/file.zip"),
        started: false,
        completed_bytes: 0,
        host: "example.com".to_string(),
        port: 2222,
        username: "testuser".to_string(),
        password: Some("secretpass".to_string()),
        remote_path: "/path/to/file.zip".to_string(),
    }
}
