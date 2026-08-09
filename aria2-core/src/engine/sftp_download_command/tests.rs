//! Tests for SFTP download command.

use std::sync::Arc;
use std::time::Duration;

use aria2_protocol::sftp::connection::SshError;
use aria2_protocol::sftp::file_ops::FileOpError;

use crate::constants;
use crate::error::{Aria2Error, FatalError, RecoverableError};
use crate::request::request_group::{DownloadOptions, GroupId, RequestGroup};

use super::types::SftpDownloadCommand;
use super::uri::sftp_percent_decode;

#[test]
fn test_sftp_path_decoding() {
    assert_eq!(sftp_percent_decode("/normal/path"), "/normal/path");
    assert_eq!(
        sftp_percent_decode("/path%20with%20spaces"),
        "/path with spaces"
    );
    // UTF-8 encoded Chinese path: "%E6%96%87%E4%BB%B6" decodes to Chinese characters for "file"
    assert_eq!(
        sftp_percent_decode("/%E6%96%87%E4%BB%B6"),
        "/\u{6587}\u{4EF6}"
    );
    assert_eq!(sftp_percent_decode("%2Froot%2Ftest"), "/root/test");
    assert_eq!(
        sftp_percent_decode("/\u{6587}\u{4EF6}"),
        "/\u{6587}\u{4EF6}"
    );
    assert_eq!(sftp_percent_decode("%5t%20"), "%5t ");
    assert_eq!(sftp_percent_decode("%"), "%");
    assert_eq!(sftp_percent_decode("%3"), "%3");
}

#[test]
fn sftp_uri_parser_matches_original_userinfo_and_path_rules() {
    let parsed = SftpDownloadCommand::parse_uri(
        "sftp://user%40name:pass%3Aword@example.com:2222/a%20file?ignored=yes#fragment",
    )
    .expect("source-compatible SFTP URI should parse");

    assert_eq!(parsed.host, "example.com");
    assert_eq!(parsed.port, 2222);
    assert_eq!(parsed.username.as_deref(), Some("user@name"));
    assert_eq!(parsed.password.as_deref(), Some("pass:word"));
    assert_eq!(parsed.remote_path, "/a file");
}

#[test]
fn sftp_uri_parser_rejects_invalid_explicit_port() {
    assert!(SftpDownloadCommand::parse_uri("sftp://host:not-a-port/file").is_err());
    assert!(SftpDownloadCommand::parse_uri("sftp://[::1]suffix/file").is_err());
}

#[test]
fn sftp_credentials_follow_original_ftp_resolution_precedence() {
    let options = DownloadOptions {
        ftp_user: Some("option-user".to_string()),
        ftp_passwd: Some("option-password".to_string()),
        no_netrc: true,
        ..DownloadOptions::default()
    };

    let option_credentials = SftpDownloadCommand::new(
        GroupId::new(34),
        "sftp://example.com/file",
        &options,
        None,
        None,
    )
    .expect("SFTP command should resolve option credentials");
    assert_eq!(option_credentials.username, "option-user");
    assert_eq!(
        option_credentials.password.as_deref(),
        Some("option-password")
    );

    let embedded_user = SftpDownloadCommand::new(
        GroupId::new(35),
        "sftp://uri-user@example.com/file",
        &options,
        None,
        None,
    )
    .expect("SFTP command should resolve a URI user with option password");
    assert_eq!(embedded_user.username, "uri-user");
    assert_eq!(embedded_user.password.as_deref(), Some("option-password"));

    let anonymous = SftpDownloadCommand::new(
        GroupId::new(36),
        "sftp://example.com/file",
        &DownloadOptions {
            no_netrc: true,
            ..DownloadOptions::default()
        },
        None,
        None,
    )
    .expect("SFTP command should resolve original anonymous fallback");
    assert_eq!(anonymous.username, "anonymous");
    assert_eq!(anonymous.password.as_deref(), Some("ARIA2USER@"));
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
        host_key_fingerprint: None,
        host: "host".to_string(),
        port: 22,
        username: "user".to_string(),
        password: None,
        remote_path: "/file".to_string(),
        global_limiter: None,
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
fn test_resume_offset_accepts_existing_prefix() {
    assert_eq!(
        SftpDownloadCommand::validate_resume_offset(256, 1024).unwrap(),
        256
    );
}

#[test]
fn test_resume_offset_rejects_oversized_local_output() {
    let error = SftpDownloadCommand::validate_resume_offset(1200, 1024).unwrap_err();
    assert!(matches!(error, Aria2Error::FileIo(_)));
}

#[test]
fn test_resume_offset_accepts_complete_output() {
    assert_eq!(
        SftpDownloadCommand::validate_resume_offset(1024, 1024).unwrap(),
        1024
    );
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
#[test]
fn engine_owned_group_constructor_preserves_gid_and_options() {
    let gid = GroupId::new(77);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec!["sftp://user:pass@example.com:2222/path/file.bin".to_string()],
        DownloadOptions::default(),
    )));
    let cmd = SftpDownloadCommand::new_with_group(
        Arc::clone(&group),
        "sftp://user:pass@example.com:2222/path/file.bin",
        &DownloadOptions::default(),
        Some("/tmp"),
        None,
    )
    .expect("group constructor should parse SFTP URI");
    assert_eq!(cmd.group().gid(), gid);
    assert_eq!(
        cmd.group().uris()[0],
        "sftp://user:pass@example.com:2222/path/file.bin"
    );
    drop(cmd);
}

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
        host_key_fingerprint: None,
        host: "example.com".to_string(),
        port: 2222,
        username: "testuser".to_string(),
        password: Some("secretpass".to_string()),
        remote_path: "/path/to/file.zip".to_string(),
        global_limiter: None,
    }
}
