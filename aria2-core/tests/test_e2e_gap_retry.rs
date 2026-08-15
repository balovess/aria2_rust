mod fixtures;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId, RequestGroup};
use aria2_core::util::rwlock_ext::RwLockRecover;
use fixtures::test_server::TestServer;
use std::path::Path;
use std::sync::Arc;

async fn start_server() -> TestServer {
    TestServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_e2e_gap_retry_skips_completed_ranges() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/retry_test.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(1),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(
        result.is_ok(),
        "Download should succeed: {:?}",
        result.err()
    );

    let output_path = Path::new(dir.path()).join("retry_test.bin");
    assert!(output_path.exists(), "Output file should exist");

    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 201, "File should be 201 bytes");

    for (i, &byte) in data.iter().enumerate().take(201) {
        assert_eq!(byte, i as u8, "Byte {} should be {}", i, i);
    }
}

#[tokio::test]
async fn test_e2e_gap_download_with_partial_progress() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/range_test.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(2),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(
        result.is_ok(),
        "Download should succeed: {:?}",
        result.err()
    );

    let output_path = dir.path().join("range_test.bin");
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 101, "File should be 101 bytes");

    for (i, &byte) in data.iter().enumerate().take(101) {
        assert_eq!(byte, i as u8, "Byte {} should be {}", i, i);
    }
}

#[tokio::test]
async fn test_e2e_gap_retry_with_server_error() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/error/500", server.base_url());

    let options = DownloadOptions {
        max_retries: 2,
        ..Default::default()
    };

    let mut cmd = DownloadCommand::new(GroupId::new(3), &url, &options, dir.path().to_str(), None)
        .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_err(), "Download should fail after retries");
    assert_eq!(
        server.error_500_requests(),
        2,
        "Configured retryable HTTP 500 should use both total attempts"
    );
}

#[tokio::test]
async fn test_e2e_http_404_is_not_retried() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/error/404", server.base_url());

    let options = DownloadOptions {
        max_retries: 3,
        retry_wait: 0,
        ..Default::default()
    };
    let mut cmd = DownloadCommand::new(GroupId::new(4), &url, &options, dir.path().to_str(), None)
        .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_err(), "404 download should fail");
    assert_eq!(
        server.error_404_requests(),
        1,
        "Non-retryable HTTP 404 should stop after the first request"
    );
}

#[tokio::test]
async fn test_merge_ranges_preserves_progress() {
    use aria2_core::engine::sequential_download::SequentialDownloader;

    let overlapping_ranges = &[(0, 100), (50, 150), (200, 50), (180, 70)];
    let merged = SequentialDownloader::merge_ranges(overlapping_ranges);

    assert_eq!(merged, vec![(0, 250)]);

    let gaps = SequentialDownloader::find_all_gaps(overlapping_ranges, 300);
    assert_eq!(gaps, vec![(250, 50)]);
}

#[tokio::test]
async fn test_find_all_gaps_with_overlapping_ranges() {
    use aria2_core::engine::sequential_download::SequentialDownloader;

    let ranges = &[(0, 200), (100, 150), (300, 50)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 500);

    assert_eq!(gaps, vec![(250, 50), (350, 150)]);
}

#[tokio::test]
async fn test_gap_download_result_partial_completion() {
    use aria2_core::engine::sequential_download::SequentialDownloader;

    let completed_ranges = &[(0, 50)];
    let gaps = SequentialDownloader::find_all_gaps(completed_ranges, 300);

    assert_eq!(gaps, vec![(50, 250)]);

    let merged = SequentialDownloader::merge_ranges(&[(0, 50), (50, 50)]);
    assert_eq!(merged, vec![(0, 100)]);

    let new_gaps = SequentialDownloader::find_all_gaps(&merged, 300);
    assert_eq!(new_gaps, vec![(100, 200)]);
}

#[tokio::test]
async fn test_gap_retry_partial_completion_logic() {
    use aria2_core::engine::sequential_download::SequentialDownloader;

    let initial_completed = &[(0, 50)];
    let gaps = SequentialDownloader::find_all_gaps(initial_completed, 251);
    assert_eq!(gaps, vec![(50, 201)]);

    let after_first_gap = SequentialDownloader::merge_ranges(&[(0, 50), (50, 50)]);
    assert_eq!(after_first_gap, vec![(0, 100)]);

    let new_gaps_after_first = SequentialDownloader::find_all_gaps(&after_first_gap, 251);
    assert_eq!(new_gaps_after_first, vec![(100, 151)]);

    let after_second_gap = SequentialDownloader::merge_ranges(&[(0, 100), (100, 50)]);
    assert_eq!(after_second_gap, vec![(0, 150)]);

    let new_gaps_after_second = SequentialDownloader::find_all_gaps(&after_second_gap, 251);
    assert_eq!(new_gaps_after_second, vec![(150, 101)]);

    let after_third_gap = SequentialDownloader::merge_ranges(&[(0, 150), (150, 101)]);
    assert_eq!(after_third_gap, vec![(0, 251)]);

    let final_gaps = SequentialDownloader::find_all_gaps(&after_third_gap, 251);
    assert!(final_gaps.is_empty());
}

#[tokio::test]
async fn test_e2e_gap_retry_skips_first_gap_after_partial_success() {
    use aria2_core::engine::sequential_download::SequentialDownloader;

    let ranges = &[(0, 100)];
    let gaps = SequentialDownloader::find_all_gaps(ranges, 300);

    assert_eq!(gaps, vec![(100, 200)]);

    let merged = SequentialDownloader::merge_ranges(&[(0, 100), (100, 50)]);
    assert_eq!(merged, vec![(0, 150)]);

    let new_gaps = SequentialDownloader::find_all_gaps(&merged, 300);
    assert_eq!(new_gaps, vec![(150, 150)]);
}

#[tokio::test]
async fn test_gap_download_result_returns_partial_completed_gaps() {
    use aria2_core::engine::sequential_download::{GapDownloadResult, SequentialDownloader};

    let completed = &[(0, 100)];
    let gaps = SequentialDownloader::find_all_gaps(completed, 300);
    assert_eq!(gaps, vec![(100, 200)]);

    let partial_completed = vec![(100, 50)];
    let result = GapDownloadResult {
        completed_gaps: partial_completed.clone(),
        error: Some(aria2_core::error::Aria2Error::Recoverable(
            aria2_core::error::RecoverableError::ServerError { code: 500 },
        )),
    };

    assert!(!result.completed_gaps.is_empty());
    assert_eq!(result.completed_gaps, partial_completed);
    assert!(result.error.is_some());

    let merged = SequentialDownloader::merge_ranges(&[(0, 100), (100, 50)]);
    assert_eq!(merged, vec![(0, 150)]);

    let remaining_gaps = SequentialDownloader::find_all_gaps(&merged, 300);
    assert_eq!(remaining_gaps, vec![(150, 150)]);
}

#[tokio::test]
async fn test_e2e_concurrent_416_fallback() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/concurrent_416_test.bin", server.base_url());

    let options = DownloadOptions {
        split: Some(5),
        ..Default::default()
    };

    let mut cmd = DownloadCommand::new(GroupId::new(5), &url, &options, dir.path().to_str(), None)
        .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "Download should succeed after fallback");

    let output_path = dir.path().join("concurrent_416_test.bin");
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 2000000, "File should be 2000000 bytes");

    let sample_positions = [0, 100000, 499999, 500000, 999999, 1000000, 1500000, 1999999];
    for &pos in &sample_positions {
        assert_eq!(
            data[pos],
            (pos % 256) as u8,
            "Byte {} should be {}",
            pos,
            pos % 256
        );
    }
}

#[tokio::test]
async fn test_e2e_concurrent_server_error_fallback() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/concurrent_server_error.bin", server.base_url());

    let options = DownloadOptions {
        split: Some(5),
        ..Default::default()
    };

    let mut cmd = DownloadCommand::new(GroupId::new(7), &url, &options, dir.path().to_str(), None)
        .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "Download should succeed after fallback");

    let output_path = dir.path().join("concurrent_server_error.bin");
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 2000000, "File should be 2000000 bytes");

    let sample_positions = [0, 100000, 499999, 500000, 999999, 1000000, 1500000, 1999999];
    for &pos in &sample_positions {
        assert_eq!(
            data[pos],
            (pos % 256) as u8,
            "Byte {} should be {}",
            pos,
            pos % 256
        );
    }
}

#[tokio::test]
async fn test_e2e_gap_retry_with_connection_disconnect() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/disconnect_range_test.bin", server.base_url());

    let options = DownloadOptions {
        max_retries: 2,
        ..Default::default()
    };

    let mut cmd = DownloadCommand::new(GroupId::new(6), &url, &options, dir.path().to_str(), None)
        .expect("Failed to create DownloadCommand");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "Download should succeed after retry");

    let output_path = dir.path().join("disconnect_range_test.bin");
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 251, "File should be 251 bytes");

    for (i, &byte) in data.iter().enumerate().take(251) {
        assert_eq!(byte, i as u8, "Byte {} should be {}", i, i);
    }
}

#[tokio::test]
async fn test_e2e_gap_pause_interrupts_stalled_body_read() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/slow_gap_test.bin", server.base_url());
    let options = DownloadOptions {
        max_retries: 1,
        split: Some(2),
        use_head: true,
        ..DownloadOptions::default()
    };
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(601),
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
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if server.slow_gap_attempts() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("concurrent download did not enter sequential gap recovery");

    group.recover_mut().pause().unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), command_task)
        .await
        .expect("pause must interrupt a stalled gap body read")
        .expect("gap download command task panicked");
    assert!(
        result.is_err(),
        "paused gap download must stop with an error"
    );
    assert!(group.recover().status().is_paused());
}

#[tokio::test]
async fn test_gap_download_result_no_error_means_complete() {
    use aria2_core::engine::sequential_download::{GapDownloadResult, SequentialDownloader};

    let completed = &[(0, 100), (100, 100)];
    let merged = SequentialDownloader::merge_ranges(completed);
    assert_eq!(merged, vec![(0, 200)]);

    let gaps = SequentialDownloader::find_all_gaps(&merged, 200);
    assert!(gaps.is_empty());

    let result = GapDownloadResult {
        completed_gaps: vec![],
        error: None,
    };

    assert!(result.completed_gaps.is_empty());
    assert!(result.error.is_none());
}

#[tokio::test]
async fn test_gap_download_result_accumulates_across_retries() {
    use aria2_core::engine::sequential_download::SequentialDownloader;

    let mut accumulated = vec![(0, 50)];
    let gaps1 = SequentialDownloader::find_all_gaps(&accumulated, 300);
    assert_eq!(gaps1, vec![(50, 250)]);

    accumulated.push((50, 50));
    accumulated = SequentialDownloader::merge_ranges(&accumulated);
    assert_eq!(accumulated, vec![(0, 100)]);

    let gaps2 = SequentialDownloader::find_all_gaps(&accumulated, 300);
    assert_eq!(gaps2, vec![(100, 200)]);

    accumulated.push((100, 100));
    accumulated = SequentialDownloader::merge_ranges(&accumulated);
    assert_eq!(accumulated, vec![(0, 200)]);

    let gaps3 = SequentialDownloader::find_all_gaps(&accumulated, 300);
    assert_eq!(gaps3, vec![(200, 100)]);

    accumulated.push((200, 100));
    accumulated = SequentialDownloader::merge_ranges(&accumulated);
    assert_eq!(accumulated, vec![(0, 300)]);

    let gaps4 = SequentialDownloader::find_all_gaps(&accumulated, 300);
    assert!(gaps4.is_empty());
}
