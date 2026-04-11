mod fixtures {
    pub mod test_metalink_builder;
    pub mod test_server;
}
use aria2_core::engine::command::Command;
use aria2_core::engine::metalink_download_command::MetalinkDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::test_metalink_builder::{
    MEDIUM_PATTERN, SMALL_CONTENT, build_metalink_v3, build_metalink_v4, compute_sha256,
};
use fixtures::test_server::TestServer;
use std::path::Path;

async fn start_server() -> TestServer {
    TestServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_e2e_metalink_parse_and_validate() {
    let server = start_server().await;
    let url = format!("{}/files/small.bin", server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml = build_metalink_v3("small.bin", 4, &[(url.clone(), 1)], &sha);
    let doc = aria2_protocol::metalink::parser::MetalinkDocument::parse(&metalink_xml).unwrap();

    assert_eq!(doc.files.len(), 1);
    assert_eq!(doc.files[0].name, "small.bin");
    assert_eq!(doc.files[0].size, Some(4));
    assert_eq!(doc.files[0].urls.len(), 1);
    assert_eq!(doc.files[0].urls[0].url, url);
    assert_eq!(doc.files[0].hashes.len(), 1);
    assert!(doc.single_file().is_some());
}

#[tokio::test]
async fn test_e2e_metalink_small_file_download() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml = build_metalink_v3("small.bin", 4, &[(url, 1)], &sha);

    let mut cmd = MetalinkDownloadCommand::new(
        GroupId::new(1),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建MetalinkDownloadCommand失败");

    cmd.execute().await.expect("Metalink下载失败");

    let output_path = Path::new(dir.path()).join("small.bin");
    assert!(
        output_path.exists(),
        "输出文件不存在: {}",
        output_path.display()
    );

    let data = std::fs::read(&output_path).expect("读取下载文件失败");
    assert_eq!(data, SMALL_CONTENT, "内容不匹配");
}

#[tokio::test]
async fn test_e2e_metalink_medium_file_download() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());
    let medium_data = vec![MEDIUM_PATTERN; 1024 * 1024];
    let sha = compute_sha256(&medium_data);

    let metalink_xml = build_metalink_v3("medium.bin", 1024 * 1024, &[(url, 1)], &sha);

    let mut cmd = MetalinkDownloadCommand::new(
        GroupId::new(2),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建MetalinkDownloadCommand失败");

    cmd.execute().await.expect("Medium文件下载失败");

    let output_path = Path::new(dir.path()).join("medium.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 1024 * 1024);
    assert!(data.iter().all(|&b| b == MEDIUM_PATTERN));
}

#[tokio::test]
async fn test_e2e_metalink_mirror_fallback() {
    let server = start_server().await;
    let dir = tmp_dir();
    let good_url = format!("{}/files/small.bin", server.base_url());
    let bad_url = format!("{}/error/500", server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml =
        build_metalink_v3("fallback_test.bin", 4, &[(bad_url, 1), (good_url, 2)], &sha);

    let mut cmd = MetalinkDownloadCommand::new(
        GroupId::new(3),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建MetalinkDownloadCommand失败");

    cmd.execute()
        .await
        .expect("镜像回退下载应成功（第一镜像500后使用第二镜像）");

    let output_path = Path::new(dir.path()).join("fallback_test.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data, SMALL_CONTENT);
}

#[tokio::test]
async fn test_e2e_metalink_v4_format_download() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml = build_metalink_v4("v4_test.bin", 4, &[(url, 1)], &sha);

    let mut cmd = MetalinkDownloadCommand::new(
        GroupId::new(4),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("V4格式Metalink创建失败");

    cmd.execute().await.expect("V4格式Metalink下载失败");

    let output_path = Path::new(dir.path()).join("v4_test.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data, SMALL_CONTENT);
}

#[tokio::test]
async fn test_e2e_metalink_invalid_input() {
    let bad_metalink = b"<metalink></metalink>".to_vec();
    let result = MetalinkDownloadCommand::new(
        GroupId::new(5),
        &bad_metalink,
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err(), "空Metalink应该返回错误");
}

#[tokio::test]
async fn test_e2e_metalink_progress_tracking() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml = build_metalink_v3("progress.bin", 4, &[(url, 1)], &sha);

    let mut cmd = MetalinkDownloadCommand::new(
        GroupId::new(6),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建MetalinkDownloadCommand失败");

    let progress_before = cmd.group().await.progress().await;
    assert!((progress_before - 0.0).abs() < 1.0, "下载前进度应为0%");

    cmd.execute().await.expect("Metalink下载失败");

    let progress_after = cmd.group().await.progress().await;
    assert!(
        (progress_after - 100.0).abs() < 1.0,
        "下载后进度应接近100%, got: {}",
        progress_after
    );

    let status = cmd.group().await.status().await;
    assert!(status.is_completed());
}
