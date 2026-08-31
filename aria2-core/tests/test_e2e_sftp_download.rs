#![cfg(feature = "sftp")]

mod fixtures;

use std::path::Path;
use std::time::Duration;

use aria2_core::engine::command::Command;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::sftp_download_command::SftpDownloadCommand;
use aria2_core::error::{Aria2Error, FatalError, RecoverableError};
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::request::request_group::{
    DownloadOptions, DownloadResultCode, DownloadStatus, GroupId, HaltReason, RequestGroup,
};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
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
        Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
    ));
}

#[tokio::test]
async fn e2e_sftp_retry_wait_is_interruptible_when_paused() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions {
        retry_wait: 5,
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        "/files/missing.bin",
        &options,
        output_dir.path(),
        "retry.bin",
        812,
    );
    let group = command
        .request_group()
        .expect("SFTP command should expose its request group");
    group
        .recover_mut()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!(2),
        )]));

    let task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if server.stat_requests() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SFTP server did not receive the first STAT request");

    group
        .recover_mut()
        .pause()
        .expect("SFTP pause should be accepted during retry-wait");
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("paused SFTP retry-wait remained asleep")
        .expect("SFTP retry task panicked");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));
    assert_eq!(group.recover().status(), DownloadStatus::Paused);
    assert_eq!(
        server.stat_requests(),
        1,
        "pause during retry-wait must prevent the next SFTP attempt"
    );
}

#[tokio::test]
async fn e2e_sftp_retry_wait_is_interruptible_when_removed() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions {
        retry_wait: 5,
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        "/files/missing.bin",
        &options,
        output_dir.path(),
        "retry-remove.bin",
        813,
    );
    let group = command
        .request_group()
        .expect("SFTP command should expose its request group");
    group
        .recover_mut()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!(2),
        )]));

    let task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if server.stat_requests() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SFTP server did not receive the first STAT request");

    group.recover_mut().mark_removed();
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("remove should interrupt SFTP retry-wait promptly")
        .expect("SFTP retry task should not panic");

    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download cancelled by user"
    ));
    assert!(group.recover().is_removed());
    assert_eq!(
        server.stat_requests(),
        1,
        "remove during retry-wait must prevent the next SFTP attempt"
    );
}

#[tokio::test]
async fn e2e_sftp_max_file_not_found_stops_after_threshold() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions::default();
    let mut command = command_for(
        &server,
        server.password(),
        "/files/missing.bin",
        &options,
        output_dir.path(),
        "download.bin",
        807,
    );
    command
        .request_group()
        .expect("SFTP command should expose its request group")
        .recover_mut()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!(2),
        )]));

    let error = execute_with_deadline(&mut command)
        .await
        .expect_err("the SFTP not-found threshold must terminate the retry loop");
    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
    ));
}

#[tokio::test]
async fn e2e_sftp_max_tries_limits_total_not_found_attempts() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions {
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        "/files/missing.bin",
        &options,
        output_dir.path(),
        "max-tries.bin",
        814,
    );
    command
        .request_group()
        .expect("SFTP command should expose its request group")
        .recover_mut()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!(3),
        )]));

    let error = execute_with_deadline(&mut command)
        .await
        .expect_err("SFTP max-tries should stop a persistent not-found response");
    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::ResourceNotFound)
    ));
    assert_eq!(
        server.stat_requests(),
        2,
        "SFTP max-tries must count total STAT attempts"
    );
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
async fn e2e_sftp_verifies_an_existing_complete_file_checksum() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_path = output_dir.path().join("verified.bin");
    std::fs::write(&output_path, server.content()).expect("complete SFTP output should be written");
    let options = DownloadOptions {
        checksum: Some((
            "sha-256".to_string(),
            "358f9c2f2bd9f0c38703ea6fdffc57414b0fdebd5c2edbd3d848a296d7e415a0".to_string(),
        )),
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &options,
        output_dir.path(),
        "verified.bin",
        809,
    );

    execute_with_deadline(&mut command)
        .await
        .expect("a complete SFTP output with a matching checksum should succeed");
    assert_eq!(command.group().status(), DownloadStatus::Complete);
    assert_eq!(
        server.read_requests(),
        0,
        "a verified complete local file must not be downloaded again"
    );
}

#[tokio::test]
async fn e2e_sftp_verifies_checksum_after_transfer() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let options = DownloadOptions {
        checksum: Some((
            "sha-256".to_string(),
            "358f9c2f2bd9f0c38703ea6fdffc57414b0fdebd5c2edbd3d848a296d7e415a0".to_string(),
        )),
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &options,
        output_dir.path(),
        "transferred.bin",
        811,
    );

    execute_with_deadline(&mut command)
        .await
        .expect("a transferred SFTP output with a matching checksum should succeed");
    assert_eq!(command.group().status(), DownloadStatus::Complete);
    assert_eq!(
        std::fs::read(output_dir.path().join("transferred.bin")).unwrap(),
        server.content()
    );
}

#[tokio::test]
async fn e2e_sftp_rejects_an_existing_complete_file_checksum_mismatch() {
    let server = MockSftpServer::start().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_path = output_dir.path().join("mismatch.bin");
    std::fs::write(&output_path, server.content()).expect("complete SFTP output should be written");
    let options = DownloadOptions {
        checksum: Some(("sha-256".to_string(), "00".repeat(32))),
        ..DownloadOptions::default()
    };
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &options,
        output_dir.path(),
        "mismatch.bin",
        810,
    );

    let error = execute_with_deadline(&mut command)
        .await
        .expect_err("a complete SFTP output with a mismatched checksum must fail");
    assert!(matches!(error, Aria2Error::Checksum(_)));
    assert_eq!(command.group().status(), DownloadStatus::Active);
    assert!(
        server.read_requests() > 0,
        "a checksum mismatch must return to the remote download path"
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
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
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
async fn e2e_sftp_pause_interrupts_stalled_read() {
    let server = MockSftpServer::start_with_read_delay_for_test(Duration::from_secs(3)).await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "stalled.bin";
    let output_path = output_dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let mut command = command_for(
        &server,
        server.password(),
        server.file_path(),
        &DownloadOptions::default(),
        output_dir.path(),
        output_name,
        809,
    );
    let group = command
        .request_group()
        .expect("SFTP command should expose its request group");
    let task = tokio::spawn(async move { command.execute().await });

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if server.read_requests() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SFTP server did not receive a READ request");

    group
        .recover_mut()
        .pause()
        .expect("SFTP pause should be accepted");
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("paused SFTP command remained in the stalled read")
        .expect("paused SFTP task panicked");

    assert!(
        result.is_err(),
        "pause should stop the current SFTP command"
    );
    assert_eq!(group.recover().status(), DownloadStatus::Paused);
    // The pause is issued before the first READ response, so no payload file
    // is required yet; the control file is the durable resume boundary.
    assert!(
        control_path.exists(),
        "pause should preserve SFTP checkpoint"
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
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
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
async fn e2e_engine_sftp_force_remove_preserves_partial_control_file() {
    let server = MockSftpServer::start_slow().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "force-removed.bin";
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
    let gid = GroupId::new(813);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![uri],
        options,
    )));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("SFTP engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_sftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_sftp_control_file(&control_path).await;
    wait_for_sftp_progress(&group).await;

    command_tx
        .send(EngineCommand::ForceRemoveDownload { gid })
        .expect("force-remove command should be accepted");
    wait_for_sftp_engine(
        engine_task,
        "force-removed SFTP download did not stop promptly",
    )
    .await;

    assert_eq!(group.read().unwrap().status(), DownloadStatus::Removed);
    assert!(
        output_path.exists(),
        "force-remove must retain partial SFTP output"
    );
    assert!(
        control_path.exists(),
        "force-remove must retain the SFTP control file"
    );
}

#[tokio::test]
async fn e2e_engine_sftp_shutdown_halt_preserves_resume_state() {
    let server = MockSftpServer::start_slow().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "shutdown-halt-sftp.bin";
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
    let gid = GroupId::new(810);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![uri],
        options,
    )));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("SFTP engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_sftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_sftp_control_file(&control_path).await;
    wait_for_sftp_progress(&group).await;

    command_tx
        .send(EngineCommand::HaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .expect("SFTP shutdown halt command should be accepted");
    wait_for_sftp_engine(engine_task, "SFTP shutdown halt did not stop the engine").await;

    let group_state = group.read().unwrap();
    assert_eq!(group_state.get_halt_reason(), HaltReason::ShutdownSignal);
    assert_ne!(
        group_state.status(),
        DownloadStatus::Removed,
        "shutdown halt must remain resumable rather than becoming a user removal"
    );
    assert_eq!(
        group_state.create_download_result().code,
        DownloadResultCode::InProgress
    );
    drop(group_state);

    assert!(
        output_path.exists(),
        "shutdown halt must retain SFTP output"
    );
    assert!(
        control_path.exists(),
        "shutdown halt must retain the SFTP control file"
    );
}

#[tokio::test]
async fn e2e_engine_sftp_force_halt_preserves_resume_state() {
    let server = MockSftpServer::start_slow().await;
    let output_dir = tempfile::tempdir().expect("temporary output directory should exist");
    let output_name = "force-halt-sftp.bin";
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
    let gid = GroupId::new(811);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![uri],
        options,
    )));
    let mut engine = DownloadEngine::new();
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("SFTP engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_sftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_sftp_control_file(&control_path).await;
    wait_for_sftp_progress(&group).await;

    command_tx
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .expect("SFTP force halt command should be accepted");
    wait_for_sftp_engine(engine_task, "SFTP force halt did not stop the engine").await;

    {
        let group_state = group.read().unwrap();
        assert_eq!(group_state.get_halt_reason(), HaltReason::ShutdownSignal);
        assert_ne!(
            group_state.status(),
            DownloadStatus::Removed,
            "force shutdown must remain resumable rather than becoming a user removal"
        );
        assert_eq!(
            group_state.create_download_result().code,
            DownloadResultCode::InProgress
        );
    }

    assert!(output_path.exists(), "force halt must retain SFTP output");
    assert!(
        control_path.exists(),
        "force halt must retain the SFTP control file"
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
