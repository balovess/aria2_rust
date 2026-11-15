use super::types::DEFAULT_BUFFER_SIZE;
use super::*;
mod edge_cases;
use crate::ftp::connection::{FtpConnection, FtpOptions};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

#[test]
fn test_human_readable_size() {
    assert_eq!(DownloadResult::human_readable_size(500), "500 B");
    assert_eq!(DownloadResult::human_readable_size(1024), "1.00 KB");
    assert_eq!(DownloadResult::human_readable_size(1536), "1.50 KB");
    assert_eq!(DownloadResult::human_readable_size(1048576), "1.00 MB");
    assert_eq!(DownloadResult::human_readable_size(1073741824), "1.00 GB");
}

#[test]
fn test_download_result_complete() {
    let full = DownloadResult {
        file_path: "test.bin".into(),
        bytes_downloaded: 1000,
        total_size: Some(1000),
        success: true,
        average_speed_bps: 1000.0,
        duration_secs: 1.0,
    };
    assert!(full.is_complete());

    let partial = DownloadResult {
        file_path: "test.bin".into(),
        bytes_downloaded: 500,
        total_size: Some(1000),
        success: true,
        average_speed_bps: 500.0,
        duration_secs: 1.0,
    };
    assert!(!partial.is_complete());

    let unknown_total = DownloadResult {
        file_path: "test.bin".into(),
        bytes_downloaded: 100,
        total_size: None,
        success: true,
        average_speed_bps: 100.0,
        duration_secs: 1.0,
    };
    assert!(unknown_total.is_complete()); // Any data with unknown total is complete

    let failed = DownloadResult {
        file_path: "test.bin".into(),
        bytes_downloaded: 1000,
        total_size: Some(1000),
        success: false, // Failed
        average_speed_bps: 1000.0,
        duration_secs: 1.0,
    };
    assert!(!failed.is_complete());
}

#[test]
fn test_ftp_download_options_default() {
    let opts = FtpDownloadOptions::default();
    assert_eq!(opts.buffer_size, DEFAULT_BUFFER_SIZE);
    assert!(opts.resume_offset.is_none());
    assert_eq!(opts.max_retries, 3);
    assert!(opts.binary_mode);
    assert_eq!(opts.data_connect_timeout, Duration::from_secs(30));
    assert!(!opts.recursive_download);
}

#[test]
fn ftp_download_buffer_size_is_bounded_at_use_time() {
    let mut options = FtpDownloadOptions::default();
    assert_eq!(options.effective_buffer_size(), DEFAULT_BUFFER_SIZE);

    options.buffer_size = 0;
    assert_eq!(options.effective_buffer_size(), 1);

    options.buffer_size = usize::MAX;
    assert_eq!(options.effective_buffer_size(), 1024 * 1024);
}

#[tokio::test]
async fn download_inherits_connection_retry_limit_unless_overridden() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = tokio::net::TcpStream::connect(address).await.unwrap();
    let (stream, _) = listener.accept().await.unwrap();
    let mut connection = FtpConnection {
        stream: BufReader::new(stream),
        options: FtpOptions {
            max_retries: 7,
            ..FtpOptions::default()
        },
        host: "127.0.0.1".to_string(),
        port: address.port(),
    };

    {
        let download = FtpDownload::new(&mut connection, None);
        assert_eq!(download.options.max_retries, 7);
    }

    let explicit_options = FtpDownloadOptions {
        max_retries: 2,
        ..FtpDownloadOptions::default()
    };
    let download = FtpDownload::new(&mut connection, Some(explicit_options));
    assert_eq!(download.options.max_retries, 2);
    drop(client);
}

#[tokio::test]
async fn failed_data_connect_aborts_transfer_and_keeps_control_session_synchronized() {
    let passive_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind passive test port");
    let passive_port = passive_listener
        .local_addr()
        .expect("read passive test port")
        .port();
    drop(passive_listener);

    let control_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP control listener");
    let control_address = control_listener
        .local_addr()
        .expect("read FTP control address");
    let server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("accept FTP control connection");
        let mut reader = BufReader::new(stream);

        for (expected, response) in [
            ("TYPE I\r\n", "200 Type set\r\n".to_string()),
            ("SIZE file\r\n", "213 1\r\n".to_string()),
            (
                "EPSV\r\n",
                format!("229 Entering Extended Passive Mode (|||{passive_port}|)\r\n"),
            ),
            (
                "RETR file\r\n",
                "150 Opening data connection\r\n".to_string(),
            ),
        ] {
            let mut command = String::new();
            reader
                .read_line(&mut command)
                .await
                .expect("read FTP command");
            assert_eq!(command, expected);
            reader
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .expect("write FTP response");
        }

        let mut telnet_abort = [0; 2];
        reader
            .read_exact(&mut telnet_abort)
            .await
            .expect("read Telnet IP and DM");
        assert_eq!(telnet_abort, [0xff, 0xf4]);
        let mut command = String::new();
        reader
            .read_line(&mut command)
            .await
            .expect("read ABOR command");
        assert_eq!(command, "ABOR\r\n");
        reader
            .get_mut()
            .write_all(b"225 Abort successful\r\n")
            .await
            .expect("write ABOR response");

        command.clear();
        reader
            .read_line(&mut command)
            .await
            .expect("read command after aborted transfer");
        assert_eq!(command, "NOOP\r\n");
        reader
            .get_mut()
            .write_all(b"200 NOOP ok\r\n")
            .await
            .expect("write NOOP response");
    });

    let stream = TcpStream::connect(control_address)
        .await
        .expect("connect FTP control stream");
    let mut connection = FtpConnection {
        stream: BufReader::new(stream),
        options: FtpOptions {
            connect_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
            ..FtpOptions::default()
        },
        host: "127.0.0.1".to_string(),
        port: control_address.port(),
    };
    let mut download = FtpDownload::new(
        &mut connection,
        Some(FtpDownloadOptions {
            data_connect_timeout: Duration::from_millis(200),
            ..FtpDownloadOptions::default()
        }),
    );
    let error = timeout(Duration::from_secs(2), download.download_to_memory("file"))
        .await
        .expect("failed data connection should not hang")
        .expect_err("closed passive port should fail the transfer");
    assert!(error.contains("FTP data connection"), "{error}");
    drop(download);

    timeout(Duration::from_secs(1), connection.noop())
        .await
        .expect("NOOP after ABOR should finish")
        .expect("NOOP should read its own response");
    timeout(Duration::from_secs(1), server)
        .await
        .expect("FTP test server should finish")
        .expect("FTP test server task should succeed");
}

#[tokio::test]
async fn local_file_setup_failure_aborts_transfer_and_preserves_control_session() {
    let data_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP data listener");
    let data_address = data_listener.local_addr().expect("read FTP data address");
    let data_server = tokio::spawn(async move {
        let (mut stream, _) = data_listener
            .accept()
            .await
            .expect("accept FTP data connection");
        let mut byte = [0; 1];
        assert_eq!(
            stream.read(&mut byte).await.expect("observe data close"),
            0,
            "client should close the data stream after local setup fails"
        );
    });

    let control_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP control listener");
    let control_address = control_listener
        .local_addr()
        .expect("read FTP control address");
    let control_server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("accept FTP control connection");
        let mut reader = BufReader::new(stream);
        let mut telnet_abort = [0; 2];
        reader
            .read_exact(&mut telnet_abort)
            .await
            .expect("read Telnet IP and DM");
        assert_eq!(telnet_abort, [0xff, 0xf4]);

        let mut command = String::new();
        reader
            .read_line(&mut command)
            .await
            .expect("read ABOR command");
        assert_eq!(command, "ABOR\r\n");
        reader
            .get_mut()
            .write_all(b"225 Abort successful\r\n")
            .await
            .expect("write ABOR response");

        command.clear();
        reader
            .read_line(&mut command)
            .await
            .expect("read command after aborted transfer");
        assert_eq!(command, "NOOP\r\n");
        reader
            .get_mut()
            .write_all(b"200 NOOP ok\r\n")
            .await
            .expect("write NOOP response");
    });

    let test_root = std::path::Path::new("target/ftp-test-tmp");
    std::fs::create_dir_all(test_root).expect("create writable FTP test root");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos();
    let output_directory = test_root.join(format!(
        "ftp-output-is-directory-{}-{unique}",
        std::process::id()
    ));
    std::fs::create_dir(&output_directory).expect("create output path as a directory");

    let stream = TcpStream::connect(control_address)
        .await
        .expect("connect FTP control stream");
    let mut connection = FtpConnection {
        stream: BufReader::new(stream),
        options: FtpOptions {
            read_timeout: Duration::from_secs(1),
            ..FtpOptions::default()
        },
        host: "127.0.0.1".to_string(),
        port: control_address.port(),
    };
    let mut download = FtpDownload::new(
        &mut connection,
        Some(FtpDownloadOptions {
            data_connect_timeout: Duration::from_secs(1),
            ..FtpDownloadOptions::default()
        }),
    );
    let error = timeout(
        Duration::from_secs(2),
        download.receive_data_to_file(
            FtpDataConnection::Passive {
                host: "127.0.0.1".to_string(),
                port: data_address.port(),
            },
            output_directory.to_str().expect("UTF-8 test path"),
            Some(1),
            None,
            None,
        ),
    )
    .await
    .expect("local setup failure should not hang")
    .expect_err("opening a directory as a file should fail");
    assert!(error.contains("Failed to create local file"), "{error}");
    drop(download);

    timeout(Duration::from_secs(1), connection.noop())
        .await
        .expect("NOOP after local setup failure should finish")
        .expect("NOOP should read its own response");
    timeout(Duration::from_secs(1), data_server)
        .await
        .expect("FTP data server should finish")
        .expect("FTP data server task should succeed");
    timeout(Duration::from_secs(1), control_server)
        .await
        .expect("FTP control server should finish")
        .expect("FTP control server task should succeed");
    std::fs::remove_dir(&output_directory).expect("remove temporary output directory");
}

#[tokio::test]
async fn preliminary_reply_is_not_accepted_as_transfer_completion() {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP control listener");
    let address = listener.local_addr().expect("read listener address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept control stream");
        stream
            .write_all(b"150 Transfer still in progress\r\n")
            .await
            .expect("write preliminary reply");
    });

    let stream = TcpStream::connect(address)
        .await
        .expect("connect FTP control stream");
    let mut connection = FtpConnection {
        stream: BufReader::new(stream),
        options: FtpOptions {
            read_timeout: Duration::from_secs(1),
            ..FtpOptions::default()
        },
        host: "127.0.0.1".to_string(),
        port: address.port(),
    };
    let mut download = FtpDownload::new(&mut connection, None);

    let error = download
        .read_transfer_complete()
        .await
        .expect_err("a preliminary 1xx response does not complete a transfer");
    assert!(error.contains("150"), "{error}");

    drop(download);
    server.await.expect("FTP test server should finish");
}

#[tokio::test]
async fn successful_226_does_not_hide_a_short_memory_download() {
    let data_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP data listener");
    let data_port = data_listener
        .local_addr()
        .expect("read FTP data port")
        .port();
    let control_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP control listener");
    let control_address = control_listener
        .local_addr()
        .expect("read FTP control address");
    let server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("accept FTP control connection");
        let mut control = BufReader::new(stream);
        for (expected, response) in [
            ("TYPE I\r\n", "200 Type set\r\n".to_string()),
            ("SIZE file\r\n", "213 2\r\n".to_string()),
            (
                "EPSV\r\n",
                format!("229 Entering Extended Passive Mode (|||{data_port}|)\r\n"),
            ),
        ] {
            let mut command = String::new();
            control
                .read_line(&mut command)
                .await
                .expect("read FTP command");
            assert_eq!(command, expected);
            control
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .expect("write FTP response");
        }

        let mut command = String::new();
        control
            .read_line(&mut command)
            .await
            .expect("read RETR command");
        assert_eq!(command, "RETR file\r\n");
        control
            .get_mut()
            .write_all(b"150 Opening data connection\r\n")
            .await
            .expect("write preliminary response");

        let (mut data, _) = data_listener
            .accept()
            .await
            .expect("accept FTP data connection");
        data.write_all(b"x").await.expect("write short payload");
        drop(data);
        control
            .get_mut()
            .write_all(b"226 Transfer complete\r\n")
            .await
            .expect("write successful completion response");

        command.clear();
        control
            .read_line(&mut command)
            .await
            .expect("read command after short transfer");
        assert_eq!(command, "NOOP\r\n");
        control
            .get_mut()
            .write_all(b"200 NOOP ok\r\n")
            .await
            .expect("write NOOP response");
    });

    let stream = TcpStream::connect(control_address)
        .await
        .expect("connect FTP control stream");
    let mut connection = FtpConnection {
        stream: BufReader::new(stream),
        options: FtpOptions {
            read_timeout: Duration::from_secs(1),
            ..FtpOptions::default()
        },
        host: "127.0.0.1".to_string(),
        port: control_address.port(),
    };
    let mut download = FtpDownload::new(
        &mut connection,
        Some(FtpDownloadOptions {
            data_connect_timeout: Duration::from_secs(1),
            ..FtpDownloadOptions::default()
        }),
    );
    let error = timeout(Duration::from_secs(2), download.download_to_memory("file"))
        .await
        .expect("short download should not hang")
        .expect_err("known remote SIZE should reject a truncated payload");
    assert!(
        error.contains("expected 2 bytes, received 1 bytes"),
        "{error}"
    );
    drop(download);

    timeout(Duration::from_secs(1), connection.noop())
        .await
        .expect("NOOP after completed short transfer should finish")
        .expect("control session should remain synchronized");
    timeout(Duration::from_secs(1), server)
        .await
        .expect("FTP test server should finish")
        .expect("FTP test server task should succeed");
}
