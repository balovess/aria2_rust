#![cfg(feature = "sftp")]

mod fixtures;

use std::path::Path;
use std::time::Duration;

use aria2_core::engine::command::Command;
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::error::{Aria2Error, FatalError};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::mock_sftp_server::MockSftpServer;

fn command_for(
    server: &MockSftpServer,
    password: &str,
    remote_path: &str,
    options: &DownloadOptions,
    output_dir: &Path,
    output_name: &str,
    gid: u64,
) -> SftpDownloadCommand {
    let uri = format!(
        "sftp://{}:{}@127.0.0.1:{}{}",
        server.username(),
        password,
        server.addr().port(),
        remote_path
    );
    SftpDownloadCommand::new(
        GroupId::new(gid),
        &uri,
        options,
        output_dir.to_str(),
        Some(output_name),
    )
    .expect("SFTP E2E command should construct")
}

async fn execute_with_deadline(command: &mut SftpDownloadCommand) -> aria2_core::error::Result<()> {
    tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("SFTP E2E operation must not hang")
}

#[tokio::test]
async fn e2e_sftp_password_authentication_downloads_the_full_file() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &DownloadOptions::default(),
        output_dir.path(),
        "download.bin",
        801,
    );

    execute_with_deadline(&mut command)
        .await
        .expect("valid SFTP password should download the file");
    assert_eq!(
        std::fs::read(output_dir.path().join("download.bin"))
            .expect("SFTP output should be readable"),
        server.content()
    );
}

#[tokio::test]
async fn e2e_sftp_rejects_an_invalid_password_before_subsystem_setup() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let mut command = command_for(
        &server,
        "wrong-password",
        server.file_path(),
        &DownloadOptions::default(),
        output_dir.path(),
        "download.bin",
        802,
    );

    let error = execute_with_deadline(&mut command)
        .await
        .expect_err("an invalid SFTP password must fail");
    assert!(matches!(
        error,
        Aria2Error::Fatal(FatalError::PermissionDenied { .. })
    ));
    assert!(!output_dir.path().join("download.bin").exists());
}

#[tokio::test]
async fn e2e_sftp_accepts_the_original_sha1_host_key_format() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions {
        ssh_host_key_md: Some(server.sha1_fingerprint().to_string()),
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &options,
        output_dir.path(),
        "download.bin",
        803,
    );

    execute_with_deadline(&mut command)
        .await
        .expect("the aria2 sha-1 host-key fingerprint must be accepted");
    assert_eq!(
        std::fs::read(output_dir.path().join("download.bin")).unwrap(),
        server.content()
    );
}

#[tokio::test]
async fn e2e_sftp_rejects_a_mismatched_host_key_fingerprint() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions {
        ssh_host_key_md: Some("sha-1=0000000000000000000000000000000000000000".to_string()),
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &options,
        output_dir.path(),
        "download.bin",
        804,
    );

    assert!(
        execute_with_deadline(&mut command).await.is_err(),
        "a mismatched SFTP host key must reject the connection"
    );
    assert!(!output_dir.path().join("download.bin").exists());
}

#[tokio::test]
async fn e2e_sftp_maps_a_missing_remote_file_to_file_not_found() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let mut command = command_for(
        &server,
        server.password(),
        "/files/missing.bin",
        &DownloadOptions::default(),
        output_dir.path(),
        "download.bin",
        805,
    );

    let error = execute_with_deadline(&mut command)
        .await
        .expect_err("a missing SFTP file must fail");
    assert!(matches!(
        error,
        Aria2Error::Fatal(FatalError::FileNotFound { .. })
    ));
}

#[tokio::test]
async fn e2e_sftp_resumes_from_an_existing_local_prefix() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_path = output_dir.path().join("resumed.bin");
    let prefix_len = server.content().len() / 2;
    std::fs::write(&output_path, &server.content()[..prefix_len])
        .expect("local SFTP resume prefix should be written");

    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &DownloadOptions::default(),
        output_dir.path(),
        "resumed.bin",
        806,
    );

    execute_with_deadline(&mut command)
        .await
        .expect("SFTP should resume from the existing output prefix");
    assert_eq!(
        std::fs::read(output_path).expect("resumed SFTP output should be readable"),
        server.content()
    );
}
