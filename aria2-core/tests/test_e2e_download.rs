mod fixtures;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::error::{Aria2Error, RecoverableError};
use aria2_core::request::request_group::{DownloadOptions, FollowMode, GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use fixtures::test_server::{TestServer, medium_pattern, small_content};
use std::path::Path;
use std::sync::Arc;

async fn start_server() -> TestServer {
    TestServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_e2e_http_download_small_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(1),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "Download failed: {:?}", result.err());

    let output_path = Path::new(dir.path()).join("small.bin");
    assert!(
        output_path.exists(),
        "Output file does not exist: {}",
        output_path.display()
    );

    let data = std::fs::read(&output_path).expect("Failed to read downloaded file");
    assert_eq!(data, small_content(), "Content mismatch");
}

#[tokio::test]
async fn test_e2e_redirect_without_location_is_http_protocol_error() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/redirect_missing", server.base_url());
    let options = DownloadOptions {
        max_retries: 3,
        retry_wait: 0,
        ..DownloadOptions::default()
    };
    let mut command =
        DownloadCommand::new(GroupId::new(2), &url, &options, dir.path().to_str(), None)
            .expect("create redirect error command");

    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("redirect error must not hang")
        .expect_err("redirect without Location must fail");
    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::HttpProtocolError { message })
            if message.contains("Location")
    ));
}

#[tokio::test]
async fn test_e2e_max_file_not_found_retries_and_preserves_result_code() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/error/404", server.base_url());
    let options = DownloadOptions {
        max_retries: 5,
        retry_wait: 0,
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2001),
        vec![url.clone()],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!("3"),
        )]));

    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url,
        &options,
        dir.path().to_str(),
        None,
    )
    .expect("create max-file-not-found command");
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("404 retry path must not hang")
        .expect_err("persistent 404 must fail");

    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
    ));
    assert_eq!(server.error_404_requests(), 3);
}

#[tokio::test]
async fn test_e2e_concurrent_max_file_not_found_preserves_result_code() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/concurrent_404_test.bin", server.base_url());
    let options = DownloadOptions {
        max_retries: 5,
        retry_wait: 0,
        split: Some(4),
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2002),
        vec![url.clone()],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!("3"),
        )]));

    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url,
        &options,
        dir.path().to_str(),
        None,
    )
    .expect("create concurrent max-file-not-found command");
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("concurrent 404 path must not hang")
        .expect_err("persistent concurrent 404 must fail");

    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
    ));
    assert!(server.error_404_requests() >= 3);
}

#[tokio::test]
async fn test_e2e_in_memory_max_file_not_found_preserves_result_code() {
    let server = start_server().await;
    let url = format!("{}/error/404", server.base_url());
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        max_retries: 5,
        retry_wait: 0,
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(2003),
        vec![url.clone()],
        options.clone(),
    )));
    group
        .write()
        .unwrap()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!("3"),
        )]));

    let mut command =
        DownloadCommand::new_with_group(Arc::clone(&group), &url, &options, None, None)
            .expect("create in-memory max-file-not-found command");
    let error = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("in-memory 404 path must not hang")
        .expect_err("persistent in-memory 404 must fail");

    assert!(matches!(
        error,
        Aria2Error::Recoverable(RecoverableError::MaxFileNotFound)
    ));
    assert_eq!(server.error_404_requests(), 3);
}

#[tokio::test]
async fn test_e2e_sequential_http_pause_interrupts_stalled_body_read() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/slow_stream_test.bin", server.base_url());
    let output = dir.path().join("slow_stream_test.bin");
    let options = DownloadOptions {
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1001),
        vec![url.clone()],
        options.clone(),
    )));
    let mut command = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &url,
        &options,
        dir.path().to_str(),
        None,
    )
    .unwrap();

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if tokio::fs::metadata(&output)
                .await
                .map(|metadata| metadata.len() > 0)
                .unwrap_or(false)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("slow HTTP stream did not write its first body chunk");

    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), command_task)
        .await
        .expect("pause must interrupt a stalled sequential HTTP body read")
        .expect("sequential HTTP command task panicked");
    assert!(result.is_err(), "paused download must stop with an error");
    assert!(group.recover().status().is_paused());

    let control_path = aria2_core::filesystem::control_file::ControlFile::control_path_for(&output);
    let checkpoint = aria2_core::filesystem::control_file::ControlFile::load(&control_path)
        .await
        .unwrap()
        .expect("pause must preserve a sequential HTTP checkpoint");
    assert!(checkpoint.completed_length() > 0);
    assert!(checkpoint.completed_length() < checkpoint.total_length());
}

#[tokio::test]
async fn test_e2e_in_memory_http_pause_interrupts_stalled_body_read() {
    let server = start_server().await;
    let url = format!("{}/files/slow_stream_test.bin", server.base_url());
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        use_head: false,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(1002),
        vec![url.clone()],
        options.clone(),
    )));
    let mut command =
        DownloadCommand::new_with_group(Arc::clone(&group), &url, &options, None, None).unwrap();

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if group.recover().completed_length() > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("slow HTTP metadata stream did not write its first body chunk");

    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), command_task)
        .await
        .expect("pause must interrupt a stalled in-memory HTTP body read")
        .expect("in-memory HTTP command task panicked");
    assert!(
        result.is_err(),
        "paused metadata download must stop with an error"
    );
    assert!(group.recover().status().is_paused());
}

#[tokio::test]
async fn test_e2e_existing_output_uses_aria2_rename_policy() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let desired = dir.path().join("small.bin");
    tokio::fs::write(&desired, b"keep-existing-file")
        .await
        .unwrap();

    let mut cmd = DownloadCommand::new(
        GroupId::new(101),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();
    cmd.execute().await.unwrap();

    assert_eq!(
        tokio::fs::read(&desired).await.unwrap(),
        b"keep-existing-file"
    );
    assert_eq!(
        tokio::fs::read(dir.path().join("small.1.bin"))
            .await
            .unwrap(),
        small_content()
    );
}

#[tokio::test]
async fn test_e2e_existing_output_can_be_rejected_or_overwritten_explicitly() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let desired = dir.path().join("small.bin");
    tokio::fs::write(&desired, b"keep-existing-file")
        .await
        .unwrap();

    let reject_options = DownloadOptions {
        auto_file_renaming: false,
        ..DownloadOptions::default()
    };
    let mut reject = DownloadCommand::new(
        GroupId::new(102),
        &url,
        &reject_options,
        dir.path().to_str(),
        None,
    )
    .unwrap();
    let error = reject
        .execute()
        .await
        .expect_err("existing file must be rejected");
    assert!(matches!(error, Aria2Error::FileAlreadyExists(_)));
    assert_eq!(
        tokio::fs::read(&desired).await.unwrap(),
        b"keep-existing-file"
    );

    let overwrite_options = DownloadOptions {
        allow_overwrite: true,
        ..DownloadOptions::default()
    };
    let mut overwrite = DownloadCommand::new(
        GroupId::new(103),
        &url,
        &overwrite_options,
        dir.path().to_str(),
        None,
    )
    .unwrap();
    overwrite.execute().await.unwrap();

    assert_eq!(tokio::fs::read(&desired).await.unwrap(), small_content());
    assert!(!dir.path().join("small.2.bin").exists());
}

#[tokio::test]
async fn test_e2e_resume_failure_honors_always_resume() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/no-range.bin", server.base_url());
    let desired = dir.path().join("no-range.bin");
    tokio::fs::write(&desired, b"resume-").await.unwrap();

    let options = DownloadOptions {
        continue_download: true,
        ..DownloadOptions::default()
    };

    let mut cmd =
        DownloadCommand::new(GroupId::new(104), &url, &options, dir.path().to_str(), None).unwrap();
    let error = cmd
        .execute()
        .await
        .expect_err("HTTP 200 for a Range request must reject resume");

    assert!(matches!(error, Aria2Error::Recoverable(_)));
    assert!(matches!(
        error,
        Aria2Error::Recoverable(aria2_core::error::RecoverableError::CannotResume)
    ));
    let data = tokio::fs::read(&desired).await.unwrap();
    assert!(data.starts_with(b"resume-"));
    assert_ne!(data, b"resume-me");
}

#[tokio::test]
async fn test_e2e_resume_failure_can_restart_when_always_resume_disabled() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/no-range.bin", server.base_url());
    let desired = dir.path().join("no-range.bin");
    tokio::fs::write(&desired, b"resume-").await.unwrap();

    let options = DownloadOptions {
        continue_download: true,
        always_resume: false,
        ..DownloadOptions::default()
    };

    let mut cmd =
        DownloadCommand::new(GroupId::new(105), &url, &options, dir.path().to_str(), None).unwrap();
    cmd.execute()
        .await
        .expect("always-resume=false should restart from byte zero");

    assert_eq!(tokio::fs::read(&desired).await.unwrap(), b"resume-me");
    assert!(!desired.with_extension("aria2").exists());
}

#[tokio::test]
async fn test_e2e_resume_failure_tries_next_mirror_before_fresh_fallback() {
    let server = start_server().await;
    let dir = tmp_dir();
    let first_uri = format!("{}/files/no-range.bin", server.base_url());
    let second_uri = format!("{}/files/resume-range.bin", server.base_url());
    let desired = dir.path().join("no-range.bin");
    tokio::fs::write(&desired, b"resume-").await.unwrap();

    let options = DownloadOptions {
        continue_download: true,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(106),
        vec![first_uri.clone(), second_uri],
        options.clone(),
    )));
    let mut cmd = DownloadCommand::new_with_group(
        Arc::clone(&group),
        &first_uri,
        &options,
        dir.path().to_str(),
        None,
    )
    .unwrap();

    cmd.execute()
        .await
        .expect("a range-capable mirror should resume the file");
    assert_eq!(tokio::fs::read(&desired).await.unwrap(), b"resume-me");
    assert_eq!(group.recover().resume_failure_count(), 1);
}

#[tokio::test]
async fn test_e2e_resume_failure_threshold_starts_from_scratch() {
    let server = start_server().await;
    let dir = tmp_dir();
    let first_uri = format!("{}/files/no-range.bin", server.base_url());
    let second_uri = format!("{}/files/resume-range.bin", server.base_url());
    let desired = dir.path().join("no-range.bin");
    tokio::fs::write(&desired, b"resume-").await.unwrap();

    let options = DownloadOptions {
        continue_download: true,
        always_resume: false,
        max_resume_failure_tries: 1,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(107),
        vec![first_uri.clone(), second_uri],
        options.clone(),
    )));
    let mut cmd =
        DownloadCommand::new_with_group(group, &first_uri, &options, dir.path().to_str(), None)
            .unwrap();

    cmd.execute()
        .await
        .expect("the threshold should trigger a fresh download");
    assert_eq!(tokio::fs::read(&desired).await.unwrap(), b"resume-me");
}

#[tokio::test]
async fn test_e2e_http_download_medium_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(2),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute().await.expect("Download failed");

    let output_path = Path::new(dir.path()).join("medium.bin");
    assert!(output_path.exists());
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 1024 * 1024);
    assert!(data.iter().all(|&b| b == medium_pattern()));
}

#[tokio::test]
async fn test_e2e_http_download_large_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/large.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(3),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute().await.expect("Large file download failed");

    let output_path = Path::new(dir.path()).join("large.bin");
    assert!(output_path.exists());
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 10 * 1024 * 1024);
}

#[tokio::test]
async fn test_e2e_http_404_handling() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/error/404", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(4),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_err(), "404 should return error");
}

#[tokio::test]
async fn test_e2e_http_500_error() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/error/500", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(5),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_err(), "500 should return error");
}

#[tokio::test]
async fn test_e2e_custom_output_dir() {
    let server = start_server().await;
    let dir = tmp_dir();
    let subdir = dir.path().join("subdir");
    let url = format!("{}/files/small.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(6),
        &url,
        &DownloadOptions::default(),
        subdir.to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute()
        .await
        .expect("Custom directory download failed");

    let output_path = subdir.join("small.bin");
    assert!(
        output_path.exists(),
        "File should be in subdirectory: {}",
        output_path.display()
    );
}

#[tokio::test]
async fn test_e2e_custom_output_filename() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(7),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        Some("custom_name.dat"),
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute()
        .await
        .expect("Custom filename download failed");

    let output_path = Path::new(dir.path()).join("custom_name.dat");
    assert!(
        output_path.exists(),
        "Custom name file does not exist: {}",
        output_path.display()
    );
}

#[tokio::test]
async fn test_e2e_request_group_progress_tracking() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(8),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let progress_before = cmd.group().progress();
    assert!(
        (progress_before - 0.0).abs() < f64::EPSILON,
        "Progress should be 0 before download"
    );

    cmd.execute().await.expect("Download failed");

    let progress_after = cmd.group().progress();
    assert!(
        (progress_after - 100.0).abs() < 1.0,
        "Progress should be near 100% after download, got: {}",
        progress_after
    );

    let status = cmd.group().status();
    assert!(status.is_completed());
}

#[tokio::test]
async fn test_e2e_download_speed_reported() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(9),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    cmd.execute().await.expect("Download failed");

    let speed = cmd.group().download_speed();
    assert!(speed > 0, "Download speed should be > 0, got: {}", speed);
}

#[tokio::test]
async fn test_e2e_concurrent_downloads() -> Result<(), Box<dyn std::error::Error>> {
    let server = start_server().await;
    let dir = tmp_dir();

    let base_url = server.base_url();
    let dir_path = dir.path().to_string_lossy().to_string();

    let mut handles = Vec::new();
    for i in 0..5u64 {
        let url = format!("{}/files/small.bin", base_url);
        let dp = dir_path.clone();
        handles.push(tokio::spawn(async move {
            let mut cmd = DownloadCommand::new(
                GroupId::new(10 + i),
                &url,
                &DownloadOptions::default(),
                Some(&dp),
                None,
            )?;
            cmd.execute().await
        }));
    }

    for h in handles {
        h.await.expect("Task panicked")?;
    }
    Ok(())
}
