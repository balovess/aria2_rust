use super::types::DEFAULT_BUFFER_SIZE;
use super::*;
use crate::ftp::connection::{FtpConnection, FtpOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
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
fn test_is_transient_io_error() {
    use std::io::ErrorKind;

    // Transient errors (should retry)
    let interrupted = std::io::Error::new(ErrorKind::Interrupted, "interrupted");
    assert!(is_transient_io_error(&interrupted));

    let would_block = std::io::Error::new(ErrorKind::WouldBlock, "would block");
    assert!(is_transient_io_error(&would_block));

    let connection_reset = std::io::Error::new(ErrorKind::ConnectionReset, "connection reset");
    assert!(is_transient_io_error(&connection_reset));

    let timed_out = std::io::Error::new(ErrorKind::TimedOut, "timed out");
    assert!(is_transient_io_error(&timed_out));

    // Non-transient errors (should not retry)
    let not_found = std::io::Error::new(ErrorKind::NotFound, "not found");
    assert!(!is_transient_io_error(&not_found));

    let permission_denied = std::io::Error::new(ErrorKind::PermissionDenied, "permission denied");
    assert!(!is_transient_io_error(&permission_denied));

    // Error with "temporary" in message
    let temp_error = std::io::Error::other("temporary failure");
    assert!(is_transient_io_error(&temp_error));
}

#[tokio::test]
async fn cancellation_aborts_an_active_ftp_download() {
    let control_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP control listener");
    let data_listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind FTP data listener");
    let control_address = control_listener
        .local_addr()
        .expect("read FTP control address");
    let data_port = data_listener
        .local_addr()
        .expect("read FTP data address")
        .port();
    let (ready_sender, ready_receiver) = oneshot::channel();

    let server = tokio::spawn(async move {
        let (stream, _) = control_listener
            .accept()
            .await
            .expect("accept FTP control connection");
        let mut control = BufReader::new(stream);

        control
            .get_mut()
            .write_all(b"220 test FTP server\r\n")
            .await
            .expect("write FTP welcome");

        let mut command = String::new();
        control
            .read_line(&mut command)
            .await
            .expect("read TYPE command");
        assert_eq!(command, "TYPE I\r\n");
        control
            .get_mut()
            .write_all(b"200 Type set\r\n")
            .await
            .expect("write TYPE response");

        command.clear();
        control
            .read_line(&mut command)
            .await
            .expect("read SIZE command");
        assert_eq!(command, "SIZE /slow.bin\r\n");
        control
            .get_mut()
            .write_all(b"213 14\r\n")
            .await
            .expect("write SIZE response");

        command.clear();
        control
            .read_line(&mut command)
            .await
            .expect("read EPSV command");
        assert_eq!(command, "EPSV\r\n");
        control
            .get_mut()
            .write_all(
                format!("229 Entering Extended Passive Mode (|||{}|)\r\n", data_port).as_bytes(),
            )
            .await
            .expect("write EPSV response");

        command.clear();
        control
            .read_line(&mut command)
            .await
            .expect("read RETR command");
        assert_eq!(command, "RETR /slow.bin\r\n");
        control
            .get_mut()
            .write_all(b"150 Opening data connection\r\n")
            .await
            .expect("write RETR response");

        let (mut data_stream, _) = data_listener
            .accept()
            .await
            .expect("accept FTP data connection");
        data_stream
            .write_all(b"partial payload")
            .await
            .expect("write partial FTP payload");
        ready_sender
            .send(())
            .expect("notify client that data transfer started");

        let mut aborted_command = Vec::new();
        control
            .read_until(b'\n', &mut aborted_command)
            .await
            .expect("read ABOR command");
        assert!(
            aborted_command
                .windows(6)
                .any(|window| window == b"ABOR\r\n")
        );
        control
            .get_mut()
            .write_all(b"226 Transfer aborted\r\n")
            .await
            .expect("write ABOR response");
    });

    let token = CancellationToken::new();
    let client_token = token.clone();
    let client = tokio::spawn(async move {
        let options = FtpOptions {
            read_timeout: Duration::from_secs(2),
            ..Default::default()
        };
        let mut connection =
            FtpConnection::connect("127.0.0.1", control_address.port(), Some(options))
                .await
                .expect("connect to test FTP server");

        FtpDownload::new(&mut connection, None)
            .download_to_memory_with_cancellation("/slow.bin", &client_token)
            .await
    });

    timeout(Duration::from_secs(1), ready_receiver)
        .await
        .expect("FTP data transfer should start")
        .expect("FTP server should notify transfer start");
    token.cancel();

    let result = timeout(Duration::from_secs(2), client)
        .await
        .expect("FTP cancellation should finish promptly")
        .expect("FTP client task should finish");
    assert_eq!(
        result.expect_err("cancelled transfer should fail"),
        "FTP download cancelled"
    );

    timeout(Duration::from_secs(2), server)
        .await
        .expect("FTP server should observe ABOR")
        .expect("FTP server task should finish");
}
