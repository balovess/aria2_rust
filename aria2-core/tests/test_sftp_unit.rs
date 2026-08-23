#![cfg(feature = "sftp")]

use aria2_core::engine::command::Command;
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::error::{Aria2Error, FatalError};
use aria2_core::request::request_group::{DownloadOptions, FollowMode, GroupId};
use aria2_core::util::rwlock_ext::RwLockRecover;

#[test]
fn test_sftp_uri_parsing_valid() {
    let result = SftpDownloadCommand::new(
        GroupId::new(1),
        "sftp://testuser@192.168.1.100:2222/path/to/file.zip",
        &DownloadOptions::default(),
        None,
        None,
    );
    assert!(result.is_ok(), "有效SFTP URI应解析成功: {:?}", result.err());
}

#[test]
fn test_sftp_uri_parsing_with_password() {
    let result = SftpDownloadCommand::new(
        GroupId::new(2),
        "sftp://admin:secret123@10.0.0.1:/data/backup.tar.gz",
        &DownloadOptions::default(),
        None,
        None,
    );
    assert!(
        result.is_ok(),
        "带密码的SFTP URI应解析成功: {:?}",
        result.err()
    );
}

#[test]
fn test_sftp_uri_parsing_default_port() {
    let result = SftpDownloadCommand::new(
        GroupId::new(3),
        "sftp://user@example.com/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    );
    assert!(result.is_ok(), "默认端口22的SFTP URI应解析成功");
}

#[test]
fn test_sftp_uri_without_user_preserves_host_and_path() {
    let uri = "sftp://127.0.0.1:2222/data/file.bin";
    let result = SftpDownloadCommand::new(
        GroupId::new(31),
        uri,
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    assert_eq!(result.group().uris(), &[uri.to_string()]);
    assert!(
        result.timeout().is_none(),
        "SFTP default options do not impose a command-level I/O timeout"
    );
}

#[test]
fn test_sftp_uri_parsing_bracketed_ipv6() {
    let result = SftpDownloadCommand::new(
        GroupId::new(32),
        "sftp://user@[::1]:2222/data/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    );
    assert!(
        result.is_ok(),
        "SFTP bracketed IPv6 URI failed: {:?}",
        result.err()
    );
}

#[test]
fn test_sftp_uri_reject_non_sftp_scheme() {
    let result = SftpDownloadCommand::new(
        GroupId::new(4),
        "http://example.com/file.zip",
        &DownloadOptions::default(),
        None,
        None,
    );
    assert!(result.is_err(), "非SFTP协议应被拒绝");
    if let Err(Aria2Error::Fatal(FatalError::UnsupportedProtocol { protocol })) = result {
        assert_eq!(protocol, "sftp");
    } else {
        panic!("应为UnsupportedProtocol错误");
    }
}

#[test]
fn test_sftp_command_custom_output_dir_and_name() {
    let result = SftpDownloadCommand::new(
        GroupId::new(5),
        "sftp://root@server.example.com:22/etc/config.conf",
        &DownloadOptions::default(),
        Some("/tmp/downloads"),
        Some("my_config.conf"),
    );
    assert!(result.is_ok());
}

#[test]
fn test_sftp_extract_filename_from_path() {
    let result = SftpDownloadCommand::new(
        GroupId::new(6),
        "sftp://user@host/deep/nested/archive.tar.bz2",
        &DownloadOptions::default(),
        Some("/out"),
        None,
    );
    assert!(result.is_ok(), "SFTP命令创建应成功");
}

#[test]
fn test_sftp_status_before_execute_is_pending() {
    let cmd = SftpDownloadCommand::new(
        GroupId::new(7),
        "sftp://user@host/file.txt",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    match cmd.status() {
        aria2_core::engine::command::CommandStatus::Pending => {}
        other => panic!("执行前状态应为Pending, got: {:?}", other),
    }
}

#[test]
fn test_sftp_memory_option_marks_request_group() {
    let options = DownloadOptions {
        follow_metalink: Some(FollowMode::Memory),
        ..DownloadOptions::default()
    };
    let cmd = SftpDownloadCommand::new(
        GroupId::new(33),
        "sftp://user@host/source.meta4",
        &options,
        Some("missing-output-dir"),
        None,
    )
    .expect("memory SFTP command should construct");

    let group = cmd
        .request_group()
        .expect("SFTP command should expose its request group");
    assert!(group.recover().is_in_memory_download());
}

#[test]
fn test_sftp_timeout_returns_value() {
    let options = DownloadOptions {
        timeout: Some(300),
        ..DownloadOptions::default()
    };
    let cmd = SftpDownloadCommand::new(
        GroupId::new(8),
        "sftp://user@host/file.txt",
        &options,
        None,
        None,
    )
    .unwrap();
    assert_eq!(cmd.timeout().unwrap().as_secs(), 300);
}
