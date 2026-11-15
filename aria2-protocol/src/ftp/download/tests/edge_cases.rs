use super::*;

#[test]
fn recursive_download_rejects_non_child_listing_paths() {
    let base = std::path::Path::new("downloads");
    assert_eq!(
        super::local_child_path(base, "subdir/file.bin").unwrap_err(),
        "Unsafe FTP listing entry name: \"subdir/file.bin\""
    );
    assert!(super::local_child_path(base, "..").is_err());
    assert!(super::local_child_path(base, "../outside.txt").is_err());
    assert!(super::local_child_path(base, "/outside.txt").is_err());
    assert_eq!(
        super::local_child_path(base, "file.bin").unwrap(),
        base.join("file.bin")
    );
}

#[test]
fn memory_download_caps_initial_allocation_for_untrusted_remote_sizes() {
    assert_eq!(super::memory_download_capacity(Some(512)), 512);
    assert_eq!(super::memory_download_capacity(None), 1024 * 1024);
    assert_eq!(super::memory_download_capacity(Some(u64::MAX)), 1024 * 1024);
}

#[test]
fn known_transfer_size_requires_an_exact_byte_count() {
    assert!(super::validate_transfer_size(Some(8), 8).is_ok());
    assert!(super::validate_transfer_size(None, 0).is_ok());
    assert!(super::validate_transfer_size(Some(8), 7).is_err());
    assert!(super::validate_transfer_size(Some(8), 9).is_err());
}

#[tokio::test]
async fn resume_target_truncates_stale_tail_and_rejects_missing_prefix() {
    let test_root = std::path::Path::new("target/ftp-test-tmp");
    std::fs::create_dir_all(test_root).expect("create writable FTP test root");
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_nanos();
    let target_path = test_root.join(format!("ftp-resume-target-{}-{unique}", std::process::id()));
    let short_path = test_root.join(format!("ftp-resume-short-{}-{unique}", std::process::id()));
    std::fs::write(&target_path, b"abcdef").expect("write stale-tail fixture");
    std::fs::write(&short_path, b"ab").expect("write short-prefix fixture");

    let mut target = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&target_path)
        .await
        .expect("open resume target");
    super::prepare_resume_target(&mut target, 3)
        .await
        .expect("truncate stale bytes after the resume offset");
    drop(target);
    assert_eq!(
        std::fs::read(&target_path).expect("read normalized resume target"),
        b"abc"
    );

    let mut short = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&short_path)
        .await
        .expect("open short resume target");
    let error = super::prepare_resume_target(&mut short, 3)
        .await
        .expect_err("resume must not create an unverified gap in the local prefix");
    assert!(error.contains("exceeds local file length"), "{error}");
    drop(short);

    std::fs::remove_file(target_path).expect("remove normalized resume target");
    std::fs::remove_file(short_path).expect("remove short resume target");
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
