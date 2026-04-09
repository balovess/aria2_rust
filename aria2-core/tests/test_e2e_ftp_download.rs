mod fixtures {
    pub mod mock_ftp_server;
}
use aria2_core::engine::command::Command;
use aria2_core::engine::ftp_download_command::FtpDownloadCommand;
use aria2_core::request::request_group::{DownloadOptions, GroupId};
use fixtures::mock_ftp_server::{medium_pattern, small_content, MockFtpServer};
use std::path::Path;

async fn start_server() -> MockFtpServer {
    MockFtpServer::start().await
}

fn tmp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
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
    let err_msg = format!("{:?}", result.err());
    assert!(
        err_msg.contains("FileNotFound") || err_msg.contains("not found"),
        "应为FileNotFound错误: {}",
        err_msg
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

    let progress_before = cmd.group().await.progress().await;
    assert!(
        (progress_before - 0.0).abs() < f64::EPSILON,
        "下载前进度应为0"
    );

    cmd.execute().await.expect("FTP下载失败");

    let progress_after = cmd.group().await.progress().await;
    assert!(
        (progress_after - 100.0).abs() < 1.0,
        "下载后进度应接近100%, got: {}",
        progress_after
    );

    let status = cmd.group().await.status().await;
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
        Some("ftp_download.dat".into()),
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
