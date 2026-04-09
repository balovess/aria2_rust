mod fixtures {
    pub mod test_metalink_builder;
    pub mod test_server;
}
use aria2_core::engine::command::{Command, CommandStatus};
use aria2_core::engine::concurrent_download_command::ConcurrentDownloadCommand;
use aria2_core::engine::concurrent_segment_manager::{ConcurrentSegmentManager, SegmentStatus};
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::test_metalink_builder::{build_metalink_v3, compute_sha256, SMALL_CONTENT};
use fixtures::test_server::TestServer;
use std::path::Path;

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_e2e_concurrent_two_mirrors() {
    let server1 = TestServer::start().await;
    let server2 = TestServer::start().await;

    let dir = tmp_dir();
    let url1 = format!("{}/files/small.bin", server1.base_url());
    let url2 = format!("{}/files/small.bin", server2.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml = build_metalink_v3("concurrent_small.bin", 4, &[(url1, 1), (url2, 1)], &sha);

    let mut cmd = ConcurrentDownloadCommand::new(
        GroupId::new(100),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建ConcurrentDownloadCommand失败");

    cmd.execute().await.expect("并发下载失败");

    let output_path = Path::new(dir.path()).join("concurrent_small.bin");
    assert!(
        output_path.exists(),
        "输出文件不存在: {}",
        output_path.display()
    );

    let data = std::fs::read(&output_path).expect("读取下载文件失败");
    assert_eq!(data, SMALL_CONTENT, "内容不匹配");
}

#[tokio::test]
async fn test_e2e_concurrent_three_mirrors() {
    let servers: Vec<TestServer> =
        futures::future::join_all((0..3).map(|_| TestServer::start()).collect::<Vec<_>>()).await;

    let dir = tmp_dir();
    let urls: Vec<(String, i32)> = (0..3)
        .map(|i| format!("{}/files/medium.bin", servers[i].base_url()))
        .map(|u| (u, 1))
        .collect();

    let medium_data = vec![0xABu8; 1024 * 1024];
    let sha = compute_sha256(&medium_data);
    let url_vecs: Vec<(String, i32)> = urls.iter().map(|(u, p)| (u.clone(), *p)).collect();

    let metalink_xml = build_metalink_v3("concurrent_medium.bin", 1024 * 1024, &url_vecs, &sha);

    let mut cmd = ConcurrentDownloadCommand::new(
        GroupId::new(101),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建ConcurrentDownloadCommand失败");

    cmd.execute().await.expect("三镜像并发下载失败");

    let output_path = Path::new(dir.path()).join("concurrent_medium.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 1024 * 1024);
    assert!(data.iter().all(|&b| b == 0xAB));
}

#[tokio::test]
async fn test_e2e_concurrent_one_mirror_fails() {
    let good_server = TestServer::start().await;
    let bad_server = TestServer::start().await;

    let dir = tmp_dir();
    let good_url = format!("{}/files/small.bin", good_server.base_url());
    let bad_url = format!("{}/error/500", bad_server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml =
        build_metalink_v3("failover_test.bin", 4, &[(bad_url, 1), (good_url, 2)], &sha);

    let mut cmd = ConcurrentDownloadCommand::new(
        GroupId::new(102),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建ConcurrentDownloadCommand失败");

    cmd.execute().await.expect("镜像故障回退应成功");

    let output_path = Path::new(dir.path()).join("failover_test.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data, SMALL_CONTENT);
}

#[tokio::test]
async fn test_e2e_concurrent_hash_verify() {
    let server = TestServer::start().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let correct_sha = compute_sha256(SMALL_CONTENT);
    let wrong_sha = "0000000000000000000000000000000000000000000000000000000000000000000";

    let metalink_xml = build_metalink_v3("hash_test.bin", 4, &[(url, 1)], &correct_sha);

    let mut cmd = ConcurrentDownloadCommand::new(
        GroupId::new(103),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建ConcurrentDownloadCommand失败");

    cmd.execute().await.expect("正确hash应通过验证");

    let output_path = Path::new(dir.path()).join("hash_test.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data, SMALL_CONTENT);
}

#[tokio::test]
async fn test_e2e_concurrent_progress_tracking() {
    let server = TestServer::start().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());
    let url2 = format!("{}/files/small.bin", server.base_url());
    let sha = compute_sha256(SMALL_CONTENT);

    let metalink_xml = build_metalink_v3("progress_test.bin", 4, &[(url, 1), (url2, 1)], &sha);

    let mut cmd = ConcurrentDownloadCommand::new(
        GroupId::new(104),
        &metalink_xml,
        &DownloadOptions::default(),
        dir.path().to_str(),
    )
    .expect("创建ConcurrentDownloadCommand失败");

    let progress_before = cmd.group().await.progress().await;
    assert!((progress_before - 0.0).abs() < 1.0, "下载前进度应为0%");

    cmd.execute().await.expect("并发下载失败");

    let status = cmd.group().await.status().await;
    assert!(status.is_completed());

    let output_path = Path::new(dir.path()).join("progress_test.bin");
    assert!(output_path.exists());

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data, SMALL_CONTENT);
}

#[tokio::test]
async fn test_e2e_concurrent_invalid_input() {
    let bad_metalink = b"<metalink></metalink>".to_vec();
    let result = ConcurrentDownloadCommand::new(
        GroupId::new(105),
        &bad_metalink,
        &DownloadOptions::default(),
        None,
    );
    assert!(result.is_err(), "空Metalink应该返回错误");
}

#[test]
fn test_segment_manager_basic() {
    let mgr = ConcurrentSegmentManager::new(
        5_000_000,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(1_000_000),
    );

    assert_eq!(mgr.num_segments(), 5);
    assert_eq!(mgr.num_mirrors(), 2);
    assert_eq!(mgr.total_size(), 5_000_000);
    assert!(!mgr.is_complete());
    assert!(mgr.has_pending_segments());
    assert!(!mgr.has_failed_segments());
}

#[test]
fn test_segment_manager_allocate_and_complete() {
    let mut mgr = ConcurrentSegmentManager::new(
        3000,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(1000),
    );

    mgr.allocate_segments();

    let done_count = (0..mgr.num_segments())
        .filter(|&i| mgr.segment_status(i) == Some(SegmentStatus::Downloading))
        .count();
    assert!(done_count > 0, "应有段被分配");
    assert!(done_count <= 3);
}

#[test]
fn test_segment_manager_fail_reassign() {
    let mut mgr = ConcurrentSegmentManager::new(
        200,
        vec!["http://a.com/f".to_string(), "http://b.com/f".to_string()],
        Some(100),
    );

    mgr.allocate_segments();

    let first_seg_idx = 0u32;
    let reassign_to = mgr.fail_segment(first_seg_idx);
    assert!(reassign_to.is_some(), "应重新分配到另一个镜像");

    let seg_status = mgr.segment_status(first_seg_idx as usize);
    assert_eq!(seg_status, Some(SegmentStatus::Pending));
    assert_eq!(
        mgr.segment_info(first_seg_idx as usize).unwrap().2,
        &SegmentStatus::Pending
    );
}

#[test]
fn test_segment_manager_assemble() {
    let mut mgr = ConcurrentSegmentManager::new(200, vec!["http://x.com/f".to_string()], Some(100));

    mgr.complete_segment(0, vec![0xAA; 100]);
    assert!((mgr.progress() - 50.0).abs() < 0.01);

    mgr.complete_segment(1, vec![0xBB; 100]);
    assert!(mgr.is_complete());
    assert!((mgr.progress() - 100.0).abs() < 0.01);

    let assembled = mgr.assemble().unwrap();
    assert_eq!(assembled.len(), 200);
    assert_eq!(&assembled[..100], &[0xAA; 100][..]);
    assert_eq!(&assembled[100..], &[0xBB; 100][..]);
}

#[test]
fn test_segment_manager_empty_file() {
    let mgr = ConcurrentSegmentManager::new(0, vec!["http://x.com/f".to_string()], None);
    assert_eq!(mgr.num_segments(), 0);
    assert!(mgr.is_complete());
    assert!(mgr.assemble().is_none());
}
