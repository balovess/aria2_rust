#![cfg(feature = "sftp")]

mod fixtures;

use std::path::Path;
use std::time::Duration;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::error::{Aria2Error, FatalError};
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::request::request_group::{DownloadOptions, DownloadStatus, GroupId, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use fixtures::mock_sftp_server::MockSftpServer;
use std::sync::Arc;

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

async fn wait_for_sftp_group_status(
    group: &Arc<std::sync::RwLock<RequestGroup>>,
    expected: DownloadStatus,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().status() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SFTP download did not reach the expected lifecycle state");
}

async fn wait_for_sftp_control_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SFTP download did not create its control file");
}

async fn wait_for_sftp_progress(group: &Arc<std::sync::RwLock<RequestGroup>>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().get_completed_length() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SFTP download did not report in-flight progress");
}

async fn wait_for_sftp_engine(
    handle: tokio::task::JoinHandle<aria2_core::error::Result<()>>,
    message: &str,
) {
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect(message)
        .expect("SFTP download engine task panicked")
        .expect("SFTP download engine returned an error");
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

#[tokio::test]
async fn e2e_engine_sftp_pause_unpause_preserves_control_file() {
    let server = MockSftpServer::start_slow().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "paused.bin";
    let output_path = output_dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let uri = format!(
        "sftp://{}:{}@127.0.0.1:{}{}",
        server.username(),
        server.password(),
        server.addr().port(),
        server.file_path()
    );
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(output_dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(807);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![uri],
        options.clone(),
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(tokio::sync::RwLock::new(RequestGroupMan::new())));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_sftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_sftp_control_file(&control_path).await;
    wait_for_sftp_progress(&group).await;

    command_tx
        .send(EngineCommand::Pause { gid })
        .expect("pause command should be accepted");
    wait_for_sftp_group_status(&group, DownloadStatus::Paused).await;
    assert!(
        output_path.exists(),
        "pause must preserve partial SFTP output"
    );
    assert!(
        control_path.exists(),
        "pause must preserve the SFTP control file"
    );

    command_tx
        .send(EngineCommand::Unpause { gid })
        .expect("unpause command should be accepted");
    wait_for_sftp_engine(
        engine_task,
        "paused SFTP download did not finish after unpause",
    )
    .await;
    assert_eq!(group.read().unwrap().status(), DownloadStatus::Complete);

    assert_eq!(std::fs::read(&output_path).unwrap(), server.content());
    assert!(
        !control_path.exists(),
        "successful SFTP completion must remove the control file"
    );
}

#[tokio::test]
async fn e2e_engine_sftp_remove_preserves_partial_control_file() {
    let server = MockSftpServer::start_slow().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "removed.bin";
    let output_path = output_dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let uri = format!(
        "sftp://{}:{}@127.0.0.1:{}{}",
        server.username(),
        server.password(),
        server.addr().port(),
        server.file_path()
    );
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(output_dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(808);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![uri],
        options,
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(tokio::sync::RwLock::new(RequestGroupMan::new())));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_sftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_sftp_control_file(&control_path).await;
    wait_for_sftp_progress(&group).await;

    command_tx
        .send(EngineCommand::RemoveDownload { gid })
        .expect("remove command should be accepted");
    wait_for_sftp_engine(engine_task, "removed SFTP download did not stop promptly").await;

    assert_eq!(group.read().unwrap().status(), DownloadStatus::Removed);
    assert!(
        output_path.exists(),
        "remove must retain partial SFTP output"
    );
    assert!(
        control_path.exists(),
        "remove must retain the SFTP control file"
    );
}

#[tokio::test]
async fn e2e_sftp_continue_false_restarts_existing_output() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "fresh-sftp.bin";
    let output_path = output_dir.path().join(output_name);
    let content = server.content();
    let prefix_len = content.len() / 2;
    std::fs::write(&output_path, vec![0xEE; prefix_len])
        .expect("existing SFTP output should be created");

    let options = DownloadOptions {
        allow_overwrite: true,
        continue_download: false,
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &options,
        output_dir.path(),
        output_name,
        809,
    );

    execute_with_deadline(&mut command)
        .await
        .expect("SFTP fresh download should succeed");
    assert_eq!(std::fs::read(output_path).unwrap(), content);
}
