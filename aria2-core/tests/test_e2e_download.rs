mod fixtures;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::test_server::{TestServer, medium_pattern, small_content};
use std::path::Path;

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

    let progress_before = cmd.group().await.progress().await;
    assert!(
        (progress_before - 0.0).abs() < f64::EPSILON,
        "Progress should be 0 before download"
    );

    cmd.execute().await.expect("Download failed");

    let progress_after = cmd.group().await.progress().await;
    assert!(
        (progress_after - 100.0).abs() < 1.0,
        "Progress should be near 100% after download, got: {}",
        progress_after
    );

    let status = cmd.group().await.status().await;
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

    let speed = cmd.group().await.download_speed().await;
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
