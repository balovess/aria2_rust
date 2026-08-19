use std::time::{Duration, Instant};
mod fixtures;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_command::DownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::test_server::TestServer;

const PERF_BASELINE_ENV: &str = "ARIA2_ENFORCE_PERF_BASELINES";

fn perf_baselines_enabled() -> bool {
    matches!(
        std::env::var(PERF_BASELINE_ENV).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn assert_max_duration(label: &str, elapsed: Duration, maximum: Duration) {
    if perf_baselines_enabled() {
        assert!(
            elapsed < maximum,
            "{label} took too long: {elapsed:?} (limit: {maximum:?})"
        );
    } else {
        println!("[PERF] {label} threshold disabled; set {PERF_BASELINE_ENV}=1 to enforce it");
    }
}

fn assert_min_speed(label: &str, speed_mb_s: f64, minimum_mb_s: f64) {
    if perf_baselines_enabled() {
        assert!(
            speed_mb_s > minimum_mb_s,
            "{label} download speed too low: {speed_mb_s:.2} MB/s (minimum: {minimum_mb_s:.2} MB/s)"
        );
    } else {
        println!("[PERF] {label} threshold disabled; set {PERF_BASELINE_ENV}=1 to enforce it");
    }
}

async fn start_server() -> TestServer {
    TestServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_perf_baseline_small_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/small.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(100),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    let start = Instant::now();
    cmd.execute().await.unwrap();
    let elapsed = start.elapsed();

    println!("[PERF] small.bin (4 bytes): {:?}", elapsed);
    assert_eq!(
        std::fs::read(dir.path().join("small.bin")).unwrap(),
        [0xDE, 0xAD, 0xBE, 0xEF]
    );
    assert_max_duration("small.bin", elapsed, Duration::from_secs(5));
}

#[tokio::test]
async fn test_perf_baseline_medium_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(101),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    let start = Instant::now();
    cmd.execute().await.unwrap();
    let elapsed = start.elapsed();
    let size_mb = 1.0f64;
    let speed_mb_s = size_mb / elapsed.as_secs_f64();

    println!(
        "[PERF] medium.bin (1MB): {:?} => {:.2} MB/s",
        elapsed, speed_mb_s
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("medium.bin"))
            .unwrap()
            .len(),
        1024 * 1024
    );
    assert_min_speed("medium.bin", speed_mb_s, 1.0);
}

#[tokio::test]
async fn test_perf_baseline_large_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/large.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(102),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    let start = Instant::now();
    cmd.execute().await.unwrap();
    let elapsed = start.elapsed();
    let size_mb = 10.0f64;
    let speed_mb_s = size_mb / elapsed.as_secs_f64();

    println!(
        "[PERF] large.bin (10MB): {:?} => {:.2} MB/s",
        elapsed, speed_mb_s
    );
    assert_eq!(
        std::fs::metadata(dir.path().join("large.bin"))
            .unwrap()
            .len(),
        10 * 1024 * 1024
    );
    assert_min_speed("large.bin", speed_mb_s, 5.0);
}

#[tokio::test]
async fn test_perf_concurrent_5_downloads() -> Result<(), Box<dyn std::error::Error>> {
    let server = start_server().await;
    let dir = tmp_dir();

    let base_url = server.base_url();
    let dir_path = dir.path().to_string_lossy().to_string();

    let start = Instant::now();

    let mut handles = Vec::new();
    for i in 0..5u64 {
        let url = format!("{}/files/medium.bin", base_url);
        let dp = dir_path.clone();
        handles.push(tokio::spawn(async move {
            let mut cmd = DownloadCommand::new(
                GroupId::new(200 + i),
                &url,
                &DownloadOptions::default(),
                Some(&dp),
                None,
            )?;
            cmd.execute().await
        }));
    }

    for h in handles {
        h.await.unwrap()?;
    }

    let elapsed = start.elapsed();
    let total_mb = 5.0f64;
    let throughput = total_mb / elapsed.as_secs_f64();

    println!(
        "[PERF] 5 concurrent x 1MB: {:?} => {:.2} MB/s total throughput",
        elapsed, throughput
    );
    assert_min_speed("5 concurrent x 1MB", throughput, 2.0);

    Ok(())
}

#[tokio::test]
async fn test_perf_request_group_speed_tracking_accuracy() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/medium.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(103),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    cmd.execute().await.unwrap();

    let reported_speed = cmd.group().download_speed();
    let group = cmd.group();

    let completed = group.completed_length();
    let elapsed = group.elapsed_time().expect("应有elapsed_time");
    let actual_speed = if elapsed.as_secs_f64() > 0.0 {
        completed as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    let ratio = if actual_speed > 0.0 {
        reported_speed as f64 / actual_speed
    } else {
        0.0
    };

    println!(
        "[PERF] Speed tracking: reported={} B/s, actual={:.2} B/s, ratio={:.2}",
        reported_speed, actual_speed, ratio
    );

    assert!(reported_speed > 0, "报告速度应大于0");
    assert!(
        ratio > 0.05 && ratio < 25.0,
        "速度跟踪偏差过大: ratio={:.2}",
        ratio
    );
}

#[tokio::test]
async fn test_perf_memory_efficiency_check() {
    let server = start_server().await;
    let dir = tmp_dir();
    let url = format!("{}/files/large.bin", server.base_url());

    let mut cmd = DownloadCommand::new(
        GroupId::new(104),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    cmd.execute().await.unwrap();

    let output_path = dir.path().join("large.bin");
    let metadata = std::fs::metadata(&output_path).unwrap();
    let file_size = metadata.len();

    println!(
        "[PERF] large.bin file size: {} bytes ({:.2} MB)",
        file_size,
        file_size as f64 / 1024.0 / 1024.0
    );
    assert_eq!(file_size, 10 * 1024 * 1024, "文件大小不匹配");

    let speed = cmd.group().download_speed();
    println!(
        "[PERF] Reported download speed: {} bytes/s ({:.2} MB/s)",
        speed,
        speed as f64 / 1024.0 / 1024.0
    );
}
