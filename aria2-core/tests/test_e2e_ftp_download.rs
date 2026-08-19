mod fixtures;
use aria2_core::engine::command::Command;
use aria2_core::engine::download_engine::DownloadEngine;
use aria2_core::engine::engine_command::EngineCommand;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
use aria2_core::error::{Aria2Error, RecoverableError};
use aria2_core::filesystem::control_file::ControlFile;
use aria2_core::request::request_group::{
    DownloadOptions, DownloadResultCode, FollowMode, GroupId, HaltReason,
};
use aria2_core::request::request_group::{DownloadStatus, RequestGroup};
use aria2_core::request::request_group_man::RequestGroupMan;
use aria2_core::util::rwlock_ext::RwLockRecover;
use fixtures::mock_ftp_server::{MockFtpServer, medium_pattern, small_content};
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn start_server() -> MockFtpServer {
    MockFtpServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

async fn wait_for_ftp_group_status(
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
    .expect("FTP download did not reach the expected lifecycle state");
}

async fn wait_for_ftp_control_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("FTP download did not create its control file");
}

async fn wait_for_ftp_progress(group: &Arc<std::sync::RwLock<RequestGroup>>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if group.read().unwrap().get_completed_length() > 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("FTP download did not report in-flight progress");
}

async fn wait_for_ftp_engine(
    handle: tokio::task::JoinHandle<aria2_core::error::Result<()>>,
    message: &str,
) {
    tokio::time::timeout(Duration::from_secs(30), handle)
        .await
        .expect(message)
        .expect("FTP download engine task panicked")
        .expect("FTP download engine returned an error");
}

#[test]
fn test_ftp_uri_parsing_bracketed_ipv6() {
    let command = FtpDownloadCommand::new(
        GroupId::new(100),
        "ftp://user:pass@[::1]:2121/path/file.bin",
        &DownloadOptions::default(),
        None,
        None,
    )
    .unwrap();
    assert!(
        command.timeout().is_none(),
        "default FTP I/O timeout is disabled; configure timeout explicitly"
    );
}

#[tokio::test]
async fn test_ftps_does_not_downgrade_to_plain_ftp() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftps://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(101),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("FTPS command should construct");

    let result = tokio::time::timeout(std::time::Duration::from_secs(2), cmd.execute()).await;
    assert!(
        result.is_ok(),
        "an ftps:// request must reject a plaintext FTP server promptly"
    );
    assert!(
        result.unwrap().is_err(),
        "an ftps:// request must not downgrade to plaintext FTP"
    );
}

#[tokio::test]
async fn test_e2e_ftp_download_small_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(1),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("创建FtpDownloadCommand失败");

    let result = cmd.execute().await;
    assert!(result.is_ok(), "FTP下载失败: {:?}", result.err());

    let output_path = Path::new(dir.path()).join("small.bin");
    assert!(
        output_path.exists(),
        "输出文件不存在: {}",
        output_path.display()
    );

    let data = std::fs::read(&output_path).expect("读取下载文件失败");
    assert_eq!(data, small_content(), "内容不匹配");
}

#[tokio::test]
async fn test_e2e_ftp_remote_time_applies_mdtm_timestamp() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());
    let options = DownloadOptions {
        remote_time: true,
        ..DownloadOptions::default()
    };

    let mut cmd =
        FtpDownloadCommand::new(GroupId::new(102), &url, &options, dir.path().to_str(), None)
            .expect("FTP remote-time command should construct");
    cmd.execute()
        .await
        .expect("FTP remote-time download should succeed");

    let actual = std::fs::metadata(dir.path().join("small.bin"))
        .expect("FTP output should exist")
        .modified()
        .expect("FTP output should expose mtime")
        .duration_since(std::time::UNIX_EPOCH)
        .expect("FTP mtime should be after the Unix epoch")
        .as_secs();
    assert!(
        actual.abs_diff(1_705_314_600) <= 1,
        "FTP remote-time should apply MDTM timestamp, got {actual}"
    );
}

#[tokio::test]
async fn test_e2e_ftp_dry_run_stops_after_metadata_probe() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());
    let options = DownloadOptions {
        dry_run: true,
        ..DownloadOptions::default()
    };

    let mut cmd =
        FtpDownloadCommand::new(GroupId::new(103), &url, &options, dir.path().to_str(), None)
            .expect("FTP dry-run command should construct");
    cmd.execute().await.expect("FTP dry-run should succeed");

    assert!(cmd.group().status().is_completed());
    assert_eq!(cmd.group().get_total_length_atomic(), 4);
    assert_eq!(cmd.group().get_completed_length(), 4);
    assert!(!dir.path().join("small.bin").exists());
}

#[tokio::test]
async fn test_e2e_ftp_connect_timeout_applies_to_silent_control_peer() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_task = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    let dir = tmp_dir();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());
    let options = DownloadOptions {
        connect_timeout: Some(1),
        max_retries: 1,
        ..DownloadOptions::default()
    };
    let mut cmd =
        FtpDownloadCommand::new(GroupId::new(104), &url, &options, dir.path().to_str(), None)
            .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(3), cmd.execute())
        .await
        .expect("FTP connect timeout should be bounded");
    assert!(result.is_err());
    accept_task.abort();
}

#[tokio::test]
async fn test_e2e_ftp_checksum_mismatch_restarts_complete_local_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let output = dir.path().join("small.bin");
    std::fs::write(&output, [0u8; 4]).unwrap();

    let options = DownloadOptions {
        checksum: Some((
            "md5".to_string(),
            "2f249230a8e7c2bf6005ccd2679259ec".to_string(),
        )),
        ..DownloadOptions::default()
    };
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());
    let mut cmd =
        FtpDownloadCommand::new(GroupId::new(105), &url, &options, dir.path().to_str(), None)
            .unwrap();

    cmd.execute().await.unwrap();
    assert_eq!(std::fs::read(output).unwrap(), small_content());
    assert!(cmd.group().status().is_completed());
}

#[tokio::test]
async fn test_e2e_ftp_rejects_short_retr_after_size() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/short.bin", addr.port());
    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(106),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    let error = cmd.execute().await.expect_err("short RETR must fail");
    assert!(
        error.to_string().contains("transfer length mismatch"),
        "unexpected FTP short-read error: {error}"
    );
}

#[tokio::test]
async fn test_e2e_ftp_protocol_failure_does_not_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicU32::new(0));
    let accepted_by_server = Arc::clone(&accepted);
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            accepted_by_server.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let mut reader = tokio::io::BufReader::new(stream);
                let writer = reader.get_mut();
                let _ = writer.write_all(b"220 retry test FTP server\r\n").await;
                let _ = writer.flush().await;

                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let verb = line.split_whitespace().next().unwrap_or("");
                    let response = match verb {
                        "USER" => b"331 Password required\r\n".as_slice(),
                        "PASS" => b"230 Login successful\r\n".as_slice(),
                        "TYPE" => b"421 Service temporarily unavailable\r\n".as_slice(),
                        _ => b"200 OK\r\n".as_slice(),
                    };
                    let writer = reader.get_mut();
                    if writer.write_all(response).await.is_err() {
                        return;
                    }
                    if writer.flush().await.is_err() || verb == "TYPE" {
                        return;
                    }
                }
            });
        }
    });

    let dir = tmp_dir();
    let options = DownloadOptions {
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    };
    let url = format!("ftp://{}/retry.bin", addr);
    let mut command =
        FtpDownloadCommand::new(GroupId::new(104), &url, &options, dir.path().to_str(), None)
            .expect("FTP command should construct");

    let result = tokio::time::timeout(std::time::Duration::from_secs(5), command.execute())
        .await
        .expect("retry test should not hang");
    assert!(result.is_err(), "persistent FTP 421 should fail");
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        1,
        "FTP protocol failures must not be retried"
    );

    server_task.abort();
}

#[tokio::test]
async fn test_e2e_ftp_max_tries_limits_total_connection_attempts() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicU32::new(0));
    let accepted_by_server = Arc::clone(&accepted);
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            accepted_by_server.fetch_add(1, Ordering::Relaxed);
            drop(stream);
        }
    });

    let dir = tmp_dir();
    let options = DownloadOptions {
        max_retries: 2,
        retry_wait: 0,
        ..DownloadOptions::default()
    };
    let url = format!("ftp://{}/max-tries.bin", addr);
    let mut command =
        FtpDownloadCommand::new(GroupId::new(107), &url, &options, dir.path().to_str(), None)
            .expect("FTP max-tries command should construct");

    let result = tokio::time::timeout(Duration::from_secs(5), command.execute())
        .await
        .expect("FTP max-tries test should not hang");
    assert!(
        result.is_err(),
        "persistent FTP connection failure must fail"
    );
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        2,
        "FTP max-tries must count total control connection attempts"
    );

    server_task.abort();
}

#[tokio::test]
async fn test_e2e_ftp_retry_wait_is_interruptible_when_paused() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicU32::new(0));
    let accepted_by_server = Arc::clone(&accepted);
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            accepted_by_server.fetch_add(1, Ordering::Relaxed);
            drop(stream);
        }
    });

    let dir = tmp_dir();
    let options = DownloadOptions {
        max_retries: 2,
        retry_wait: 5,
        ..DownloadOptions::default()
    };
    let uri = format!("ftp://{}/retry-wait.bin", addr);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(105),
        vec![uri],
        options,
    )));
    let mut command = FtpDownloadCommand::new_with_group(
        Arc::clone(&group),
        dir.path().to_str(),
        Some("retry-wait.bin"),
    )
    .expect("FTP retry-wait command should construct");

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while accepted.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("FTP command should reach the transient control failure");

    tokio::time::sleep(Duration::from_millis(100)).await;
    group.write().unwrap().pause().unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), command_task)
        .await
        .expect("pause should interrupt FTP retry-wait promptly")
        .expect("FTP retry-wait task should not panic");
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download paused"
    ));

    server_task.abort();
}

#[tokio::test]
async fn test_e2e_ftp_retry_wait_is_interruptible_when_removed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accepted = Arc::new(AtomicU32::new(0));
    let accepted_by_server = Arc::clone(&accepted);
    let server_task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            accepted_by_server.fetch_add(1, Ordering::Relaxed);
            drop(stream);
        }
    });

    let dir = tmp_dir();
    let options = DownloadOptions {
        max_retries: 2,
        retry_wait: 5,
        ..DownloadOptions::default()
    };
    let uri = format!("ftp://{}/retry-wait-remove.bin", addr);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        GroupId::new(106),
        vec![uri],
        options,
    )));
    let mut command = FtpDownloadCommand::new_with_group(
        Arc::clone(&group),
        dir.path().to_str(),
        Some("retry-wait-remove.bin"),
    )
    .expect("FTP retry-wait remove command should construct");

    let command_task = tokio::spawn(async move { command.execute().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while accepted.load(Ordering::Relaxed) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("FTP command should reach the transient control failure");

    tokio::time::sleep(Duration::from_millis(100)).await;
    group.recover_mut().mark_removed();
    let result = tokio::time::timeout(Duration::from_secs(1), command_task)
        .await
        .expect("remove should interrupt FTP retry-wait promptly")
        .expect("FTP retry-wait remove task should not panic");
    assert!(matches!(
        result,
        Err(Aria2Error::DownloadFailed(message)) if message == "Download cancelled by user"
    ));
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        1,
        "remove during retry-wait must prevent the next FTP attempt"
    );

    server_task.abort();
}

#[tokio::test]
async fn test_e2e_ftp_pasv_uses_control_peer_when_host_is_misadvertised() {
    let server = MockFtpServer::start_with_pasv_advertised_host([127, 0, 0, 2]).await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(102),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("FTP command should construct");

    cmd.execute()
        .await
        .expect("PASV data connection should use the control peer");
    assert_eq!(
        std::fs::read(dir.path().join("small.bin")).unwrap(),
        small_content()
    );
}

#[tokio::test]
async fn test_e2e_ftp_memory_download_keeps_source_out_of_filesystem() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());
    let options = DownloadOptions {
        follow_torrent: Some(FollowMode::Memory),
        ..DownloadOptions::default()
    };

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(9),
        &url,
        &options,
        dir.path().to_str(),
        Some("source.torrent"),
    )
    .expect("memory FTP command should construct");

    cmd.execute()
        .await
        .expect("memory FTP download should succeed");

    assert!(!dir.path().join("source.torrent").exists());
    let group = cmd
        .request_group()
        .expect("FTP command should expose its request group");
    let group = group.recover();
    assert!(group.is_in_memory_download());
    assert_eq!(group.in_memory_data(), Some(small_content().to_vec()));
    assert_eq!(group.total_length(), small_content().len() as u64);
    assert!(group.status().is_completed());
}

#[tokio::test]
async fn test_e2e_ftp_download_medium_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(2),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("创建FtpDownloadCommand失败");

    cmd.execute().await.expect("FTP medium文件下载失败");

    let output_path = Path::new(dir.path()).join("medium.bin");
    assert!(output_path.exists());
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 1024 * 1024);
    assert!(data.iter().all(|&b| b == medium_pattern()));
}

#[tokio::test]
async fn test_e2e_ftp_download_large_file() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/large.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(3),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("创建FtpDownloadCommand失败");

    cmd.execute().await.expect("FTP large文件下载失败");

    let output_path = Path::new(dir.path()).join("large.bin");
    assert!(output_path.exists());
    let data = std::fs::read(&output_path).unwrap();
    assert_eq!(data.len(), 10 * 1024 * 1024);
}

#[tokio::test]
async fn test_e2e_ftp_binary_mode_correctness() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(4),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .unwrap();

    cmd.execute().await.unwrap();

    let data = std::fs::read(dir.path().join("small.bin")).unwrap();
    assert_eq!(
        data,
        &[0xDE, 0xAD, 0xBE, 0xEF],
        "二进制模式应保持原始字节不变"
    );
}

#[tokio::test]
async fn test_e2e_ftp_550_not_found() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/notfound", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(5),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("创建FtpDownloadCommand失败");

    let result = cmd.execute().await;
    assert!(result.is_err(), "550应返回错误");
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::ResourceNotFound))
    ));
}

#[tokio::test]
async fn test_e2e_ftp_max_file_not_found_stops_after_threshold() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/notfound", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(6),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("FtpDownloadCommand should construct");
    cmd.request_group()
        .expect("FTP command should expose its request group")
        .recover_mut()
        .set_option_snapshot(std::collections::HashMap::from([(
            "max-file-not-found".to_string(),
            serde_json::json!(2),
        )]));

    let result = cmd.execute().await;
    assert!(matches!(
        result,
        Err(Aria2Error::Recoverable(RecoverableError::MaxFileNotFound))
    ));
}

#[tokio::test]
async fn test_e2e_ftp_active_mode_download() {
    let server = MockFtpServer::start_active().await;
    let dir = tmp_dir();
    let options = DownloadOptions {
        ftp_pasv: false,
        ..DownloadOptions::default()
    };
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", server.addr().port());
    let mut command =
        FtpDownloadCommand::new(GroupId::new(103), &url, &options, dir.path().to_str(), None)
            .expect("active FTP command should construct");

    command
        .execute()
        .await
        .expect("active FTP download should complete");
    assert_eq!(
        std::fs::read(dir.path().join("small.bin")).unwrap(),
        small_content()
    );
}

#[tokio::test]
async fn test_e2e_ftp_download_uses_pwd_cwd_before_file_commands() {
    let server = MockFtpServer::start_requires_cwd().await;
    let dir = tmp_dir();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", server.addr().port());
    let mut command = FtpDownloadCommand::new(
        GroupId::new(107),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("FTP command should construct");

    command
        .execute()
        .await
        .expect("FTP download should use CWD before SIZE and RETR");
    assert_eq!(
        std::fs::read(dir.path().join("small.bin")).unwrap(),
        small_content()
    );
}

#[tokio::test]
async fn test_e2e_ftp_request_group_progress_tracking() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(6),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        None,
    )
    .expect("创建FtpDownloadCommand失败");

    let progress_before = cmd.group().progress();
    assert!(
        (progress_before - 0.0).abs() < f64::EPSILON,
        "下载前进度应为0"
    );

    cmd.execute().await.expect("FTP下载失败");

    let progress_after = cmd.group().progress();
    assert!(
        (progress_after - 100.0).abs() < 1.0,
        "下载后进度应接近100%, got: {}",
        progress_after
    );

    let status = cmd.group().status();
    assert!(status.is_completed());
}

#[tokio::test]
async fn test_e2e_ftp_custom_output_dir() {
    let server = start_server().await;
    let dir = tmp_dir();
    let subdir = dir.path().join("ftp_subdir");
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(7),
        &url,
        &DownloadOptions::default(),
        subdir.to_str(),
        None,
    )
    .expect("创建FtpDownloadCommand失败");

    cmd.execute().await.expect("FTP自定义目录下载失败");

    let output_path = subdir.join("small.bin");
    assert!(
        output_path.exists(),
        "文件应在FTP子目录中: {}",
        output_path.display()
    );
}

#[tokio::test]
async fn test_e2e_ftp_custom_output_filename() {
    let server = start_server().await;
    let dir = tmp_dir();
    let addr = server.addr();
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());

    let mut cmd = FtpDownloadCommand::new(
        GroupId::new(8),
        &url,
        &DownloadOptions::default(),
        dir.path().to_str(),
        Some("ftp_download.dat"),
    )
    .expect("创建FtpDownloadCommand失败");

    cmd.execute().await.expect("FTP自定义文件名下载失败");

    let output_path = Path::new(dir.path()).join("ftp_download.dat");
    assert!(
        output_path.exists(),
        "自定义FTP名称文件不存在: {}",
        output_path.display()
    );
}

#[tokio::test]
async fn test_e2e_ftp_concurrent_downloads() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tmp_dir();
    let dir_path = dir.path().to_string_lossy().to_string();

    let mut handles = Vec::new();
    for i in 0..3u64 {
        let dp = dir_path.clone();
        handles.push(tokio::spawn(async move {
            let server = start_server().await;
            let addr = server.addr();
            let url = format!("ftp://127.0.0.1:{}/files/small.bin", addr.port());
            let mut cmd = FtpDownloadCommand::new(
                GroupId::new(20 + i),
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
    Ok(())
}

#[tokio::test]
async fn test_engine_ftp_pause_unpause_preserves_control_file() {
    let server = MockFtpServer::start_slow().await;
    let dir = tmp_dir();
    let output_name = "medium.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", server.addr().port());
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(407);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_ftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_ftp_control_file(&control_path).await;
    wait_for_ftp_progress(&group).await;

    command_tx
        .send(EngineCommand::Pause { gid })
        .expect("pause command should be accepted");
    wait_for_ftp_group_status(&group, DownloadStatus::Paused).await;
    wait_for_ftp_control_file(&control_path).await;
    assert!(
        output_path.exists(),
        "pause must preserve partial FTP output"
    );

    command_tx
        .send(EngineCommand::Unpause { gid })
        .expect("unpause command should be accepted");
    wait_for_ftp_engine(
        engine_task,
        "paused FTP download did not finish after unpause",
    )
    .await;
    assert_eq!(group.read().unwrap().status(), DownloadStatus::Complete);

    let downloaded = std::fs::read(&output_path).expect("FTP output should be readable");
    let expected = vec![medium_pattern(); 1024 * 1024];
    assert_eq!(
        downloaded.len(),
        expected.len(),
        "resumed FTP output length"
    );
    let first_mismatch = downloaded
        .iter()
        .zip(&expected)
        .position(|(actual, expected)| actual != expected);
    assert_eq!(
        first_mismatch, None,
        "resumed FTP output differs at byte {:?}",
        first_mismatch
    );
    assert!(
        !control_path.exists(),
        "successful FTP completion must remove the control file"
    );
}

#[tokio::test]
async fn test_e2e_ftp_pause_interrupts_stalled_data_read() {
    let server = MockFtpServer::start_with_transfer_delay(Duration::from_secs(3)).await;
    let dir = tmp_dir();
    let output_name = "stalled.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", server.addr().port());
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let mut command = FtpDownloadCommand::new(
        GroupId::new(410),
        &url,
        &options,
        Some(dir.path().to_str().unwrap()),
        Some(output_name),
    )
    .expect("FTP command should construct");
    let group = command
        .request_group()
        .expect("FTP command should expose its request group");
    let task = tokio::spawn(async move { command.execute().await });

    wait_for_ftp_progress(&group).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    group
        .recover_mut()
        .pause()
        .expect("FTP pause should be accepted");
    let result = tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .expect("paused FTP command remained in the stalled data read")
        .expect("paused FTP task panicked");

    assert!(result.is_err(), "pause should stop the current FTP command");
    assert_eq!(group.recover().status(), DownloadStatus::Paused);
    assert!(output_path.exists(), "pause should preserve FTP output");
    assert!(
        control_path.exists(),
        "pause should preserve FTP checkpoint"
    );
}

#[tokio::test]
async fn test_engine_ftp_remove_preserves_partial_control_file() {
    let server = MockFtpServer::start_slow().await;
    let dir = tmp_dir();
    let output_name = "medium.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", server.addr().port());
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(408);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_ftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_ftp_control_file(&control_path).await;
    wait_for_ftp_progress(&group).await;

    command_tx
        .send(EngineCommand::RemoveDownload { gid })
        .expect("remove command should be accepted");
    wait_for_ftp_engine(engine_task, "removed FTP download did not stop promptly").await;

    assert_eq!(group.read().unwrap().status(), DownloadStatus::Removed);
    assert!(
        output_path.exists(),
        "remove must retain partial FTP output"
    );
    assert!(
        control_path.exists(),
        "remove must retain the FTP control file"
    );
}

#[tokio::test]
async fn test_engine_ftp_force_remove_preserves_partial_control_file() {
    let server = MockFtpServer::start_slow().await;
    let dir = tmp_dir();
    let output_name = "force-removed.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", server.addr().port());
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(413);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_ftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_ftp_control_file(&control_path).await;
    wait_for_ftp_progress(&group).await;

    command_tx
        .send(EngineCommand::ForceRemoveDownload { gid })
        .expect("force-remove command should be accepted");
    wait_for_ftp_engine(
        engine_task,
        "force-removed FTP download did not stop promptly",
    )
    .await;

    assert_eq!(group.read().unwrap().status(), DownloadStatus::Removed);
    assert!(
        output_path.exists(),
        "force-remove must retain partial FTP output"
    );
    assert!(
        control_path.exists(),
        "force-remove must retain the FTP control file"
    );
}

#[tokio::test]
async fn test_engine_ftp_shutdown_halt_preserves_resume_state() {
    let server = MockFtpServer::start_slow().await;
    let dir = tmp_dir();
    let output_name = "shutdown-halt-ftp.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", server.addr().port());
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(411);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("FTP engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_ftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_ftp_control_file(&control_path).await;
    wait_for_ftp_progress(&group).await;

    command_tx
        .send(EngineCommand::HaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .expect("FTP shutdown halt command should be accepted");
    wait_for_ftp_engine(engine_task, "FTP shutdown halt did not stop the engine").await;

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

    assert!(output_path.exists(), "shutdown halt must retain FTP output");
    assert!(
        control_path.exists(),
        "shutdown halt must retain the FTP control file"
    );
}

#[tokio::test]
async fn test_engine_ftp_force_halt_preserves_resume_state() {
    let server = MockFtpServer::start_slow().await;
    let dir = tmp_dir();
    let output_name = "force-halt-ftp.bin";
    let output_path = dir.path().join(output_name);
    let control_path = ControlFile::control_path_for(&output_path);
    let url = format!("ftp://127.0.0.1:{}/files/medium.bin", server.addr().port());
    let options = DownloadOptions {
        continue_download: true,
        allow_overwrite: true,
        dir: Some(dir.path().to_string_lossy().into_owned()),
        out: Some(output_name.to_string()),
        ..DownloadOptions::default()
    };
    let gid = GroupId::new(412);
    let group = Arc::new(std::sync::RwLock::new(RequestGroup::new(
        gid,
        vec![url],
        options,
    )));
    let mut engine = DownloadEngine::new(5);
    engine.set_request_group_man(Arc::new(RequestGroupMan::new()));
    let command_tx = engine.engine_command_sender();
    command_tx
        .send(EngineCommand::AddDownload {
            group: Arc::clone(&group),
        })
        .expect("FTP engine command channel should be open");
    let engine_task = tokio::spawn(engine.run());

    wait_for_ftp_group_status(&group, DownloadStatus::Active).await;
    wait_for_ftp_control_file(&control_path).await;
    wait_for_ftp_progress(&group).await;

    command_tx
        .send(EngineCommand::ForceHaltAll {
            reason: HaltReason::ShutdownSignal,
        })
        .expect("FTP force halt command should be accepted");
    wait_for_ftp_engine(engine_task, "FTP force halt did not stop the engine").await;

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

    assert!(output_path.exists(), "force halt must retain FTP output");
    assert!(
        control_path.exists(),
        "force halt must retain the FTP control file"
    );
}

#[tokio::test]
async fn test_e2e_ftp_continue_false_restarts_existing_output() {
    let server = start_server().await;
    let dir = tmp_dir();
    let output_name = "fresh-ftp.bin";
    let output_path = dir.path().join(output_name);
    let expected = small_content();
    let prefix_len = expected.len() / 2;
    std::fs::write(&output_path, vec![0xEE; prefix_len])
        .expect("existing FTP output should be created");

    let options = DownloadOptions {
        allow_overwrite: true,
        continue_download: false,
        ..DownloadOptions::default()
    };
    let url = format!("ftp://127.0.0.1:{}/files/small.bin", server.addr().port());
    let mut command = FtpDownloadCommand::new(
        GroupId::new(409),
        &url,
        &options,
        Some(dir.path().to_str().unwrap()),
        Some(output_name),
    )
    .expect("FTP command should construct");

    command
        .execute()
        .await
        .expect("FTP fresh download should succeed");
    assert_eq!(std::fs::read(output_path).unwrap(), expected);
}

#[tokio::test]
async fn test_raw_tcp_connectivity() {
    use tokio::io::AsyncBufReadExt;
    let server = start_server().await;
    let addr = server.addr();
    let port = addr.port();

    println!("[DIAG] MockFtpServer listening on {}", addr);

    let mut stream = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .expect("TCP连接应成功");
    println!("[DIAG] TCP connected to {}:{}", addr.ip(), port);

    let mut reader = tokio::io::BufReader::new(&mut stream);
    let mut line = String::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        reader.read_line(&mut line),
    )
    .await;

    match result {
        Ok(Ok(n)) => println!("[DIAG] 读取到 {} bytes: {:?}", n, line.trim()),
        Ok(Err(e)) => panic!("[DIAG] 读取错误: {}", e),
        Err(_) => panic!("[DIAG] 5秒内未读取到数据! 服务器未发送问候语"),
    }
}
