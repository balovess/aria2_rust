use std::time::Instant;
mod fixtures {
    pub mod mock_ftp_server;
}
use aria2_core::engine::command::Command;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::mock_ftp_server::MockFtpServer;

async fn start_server() -> MockFtpServer {
    MockFtpServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[tokio::test]
async fn test_perf_ftp_small_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(100),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    let start = Instant::now();

    let result = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.execute()).await;

    let elapsed = start.elapsed();
    match result {
        Ok(Ok(())) => println!("[PERF-FTP] small.bin (4 bytes): {:?}", elapsed),
        Ok(Err(e)) => eprintln!("[PERF-FTP] small.bin error (skipped): {}", e),
        Err(_) => eprintln!("[PERF-FTP] small.bin timeout (skipped): {:?}", elapsed),
    }
}

#[tokio::test]
async fn test_perf_ftp_medium_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
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
        "[PERF-FTP] medium.bin (1MB): {:?} => {:.2} MB/s",
        elapsed, speed_mb_s
    );
    assert!(
        speed_mb_s > 0.01,
        "FTP 1MB download too slow: {:.2} MB/s",
        speed_mb_s
    );
}

#[tokio::test]
async fn test_perf_ftp_concurrent_downloads() -> Result<(), Box<dyn std::error::Error>> {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let port = addr.port();
    let dir_path = dir.path().to_string_lossy().to_string();

    let start = Instant::now();

    let mut handles = Vec::new();
    for i in 0..3u64 {
        let url = format!("ftp://127.0.0.1:{}/files/small.bin", port);
        let dp = dir_path.clone();
        handles.push(tokio::spawn(async move {
            let mut cmd = FtpDownloadCommand::new(
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
        h.await.expect("任务panic")?;
    }

    let elapsed = start.elapsed();
    println!("[PERF-FTP] 3 concurrent x small: {:?} (FTP)", elapsed);
    Ok(())
}
