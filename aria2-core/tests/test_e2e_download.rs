mod fixtures {
    pub mod test_server;
}
use fixtures::test_server::{TestServer, small_content, medium_pattern};
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::engine::command::Command;
use aria2_core::request::request_group::{GroupId, DownloadOptions};
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
    ).expect("创建DownloadCommand失败");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "下载失败: {:?}", result.err());

    let output_path = Path::new(dir.path()).join("small.bin");
    assert!(output_path.exists(), "输出文件不存在: {}", output_path.display());

    let data = std::fs::read(&output_path).expect("读取下载文件失败");
    assert_eq!(data, small_content(), "内容不匹配");
}

#[tokio::test]
async fn test_e2e_http_download_medium_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(2), &url, &DownloadOptions::default(),
        dir.path().to_str(), None,
    ).expect("创建DownloadCommand失败");

    cmd.execute().await.expect("下载失败");

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
        GroupId::new(3), &url, &DownloadOptions::default(),
        dir.path().to_str(), None,
    ).expect("创建DownloadCommand失败");

    cmd.execute().await.expect("大文件下载失败");

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
        GroupId::new(4), &url, &DownloadOptions::default(),
        dir.path().to_str(), None,
    ).expect("创建DownloadCommand失败");

    let result = cmd.execute().await;
    assert!(result.is_err(), "404应该返回错误");
}

#[tokio::test]
async fn test_e2e_http_500_error() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/error/500", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(5), &url, &DownloadOptions::default(),
        dir.path().to_str(), None,
    ).expect("创建DownloadCommand失败");

    let result = cmd.execute().await;
    assert!(result.is_err(), "500应该返回错误");
}

#[tokio::test]
async fn test_e2e_custom_output_dir() {
    let server = start_server().await;
    let dir = tmp_dir();
    let subdir = dir.path().join("subdir");
    let url = format!("{}/files/small.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(6), &url, &DownloadOptions::default(),
        subdir.to_str(), None,
    ).expect("创建DownloadCommand失败");

    cmd.execute().await.expect("自定义目录下载失败");

    let output_path = subdir.join("small.bin");
    assert!(output_path.exists(), "文件应在子目录中: {}", output_path.display());
}

#[tokio::test]
async fn test_e2e_custom_output_filename() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(7), &url, &DownloadOptions::default(),
        dir.path().to_str(),
        Some("custom_name.dat".into()),
    ).expect("创建DownloadCommand失败");

    cmd.execute().await.expect("自定义文件名下载失败");

    let output_path = Path::new(dir.path()).join("custom_name.dat");
    assert!(output_path.exists(), "自定义名称文件不存在: {}", output_path.display());
}

#[tokio::test]
async fn test_e2e_request_group_progress_tracking() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(8), &url, &DownloadOptions::default(),
        dir.path().to_str(), None,
    ).expect("创建DownloadCommand失败");

    let progress_before = cmd.group().await.progress().await;
    assert!((progress_before - 0.0).abs() < f64::EPSILON, "下载前进度应为0");

    cmd.execute().await.expect("下载失败");

    let progress_after = cmd.group().await.progress().await;
    assert!((progress_after - 100.0).abs() < 1.0, "下载后进度应接近100%, got: {}", progress_after);

    let status = cmd.group().await.status().await;
    assert!(status.is_completed());
}

#[tokio::test]
async fn test_e2e_download_speed_reported() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(9), &url, &DownloadOptions::default(),
        dir.path().to_str(), None,
    ).expect("创建DownloadCommand失败");

    cmd.execute().await.expect("下载失败");

    let speed = cmd.group().await.download_speed().await;
    assert!(speed > 0, "下载速度应大于0, got: {}", speed);
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
                GroupId::new(10 + i), &url, &DownloadOptions::default(),
                Some(&dp), None,
            )?;
            cmd.execute().await
        }));
    }

    for h in handles {
        h.await.expect("任务panic")?;
    }
    Ok(())
}
